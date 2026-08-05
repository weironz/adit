use super::*;

pub(crate) fn update(app: &mut AditApp, message: Message) -> Task<Message> {
    match message {
        Message::Tick => {
            app.manager.poll_events();
            auto_log_connected_sessions(app);
            clamp_terminal_scroll(app);
            sync_auth_prompt(app);
            sync_auth_retry(app);
            sync_sftp_state(app);
            // Reconcile split panes with the live session set (closed sessions,
            // an externally-activated session); refit only if the count changed.
            // Profile saves are queued to a writer thread, so a disk failure lands
            // long after the call returned Ok. Surface it instead of letting the
            // user believe their sessions were saved.
            if let Some(error) = app.profile_store.take_write_error() {
                app.last_error = Some(format!("保存会话配置失败: {error}"));
            }
            let panes_before = app.panes.len();
            sync_panes(app);
            if app.panes.len() != panes_before {
                sync_terminal_size(app);
            }
            persist_settings_if_changed(app);

            // RDP clipboard bridge: the helper is a windowless process, so only
            // this one has a Windows clipboard. Remote copies land here...
            if let Some(text) = app.manager.take_rdp_clipboard().filter(|_| app.rdp_clipboard) {
                // Record it as already-offered so the poll below doesn't bounce
                // the remote's own text straight back at it.
                app.rdp_clipboard_offered = Some(text.clone());
                return clipboard::write(text);
            }
            // ...and the local clipboard is sampled for the remote. Only while an
            // RDP tab is in front, and only every RDP_CLIPBOARD_POLL_TICKS: a read
            // opens the system clipboard, and doing that ten times a second would
            // fight every other app on the machine for it.
            if app.rdp_clipboard && app.manager.active_is_rdp() {
                app.rdp_clipboard_ticks = app.rdp_clipboard_ticks.wrapping_add(1);
                if app.rdp_clipboard_ticks.is_multiple_of(RDP_CLIPBOARD_POLL_TICKS) {
                    return clipboard::read().map(Message::RdpClipboardPolled);
                }
            }
        }
        Message::RdpClipboardPolled(contents) => {
            // Offer only what actually changed: the poll fires on a timer, and
            // the offer costs a FormatList on the RDP wire.
            if let Some(text) = contents.filter(|text| !text.is_empty()) {
                if app.rdp_clipboard_offered.as_deref() != Some(text.as_str()) {
                    app.rdp_clipboard_offered = Some(text.clone());
                    app.manager.offer_clipboard_to_active_rdp(text);
                }
            }
        }
        Message::RdpTick => {
            // The cached frame belongs to one session; if the active session
            // changed (tab switch, or a close that auto-activated another tab),
            // drop the cache so we never paint one host's frame under another's
            // tab. Each RDP session has its own generation counter, so comparing
            // across sessions would otherwise be meaningless.
            let active = app.manager.active_session();
            if active != app.rdp_frame_session {
                app.rdp_frame_session = active;
                app.rdp_frame_generation = 0;
                app.rdp_tiles.clear();
                app.rdp_surface_size = None;
                // Different session ⇒ forget the requested size so the next layout
                // sync re-asserts the viewport for the newly-active desktop.
                app.rdp_target_size = None;
            }
            // Sample the active RDP framebuffer; rebuild the GPU image handle only
            // when the helper produced a new generation.
            // Throttle the GPU handle rebuild.
            //
            // `image::Handle::from_rgba` mints a fresh `Id` every call, and the
            // renderer's raster cache evicts any id not hit in the current
            // frame — so every rebuild is a cache miss that must go through
            // iced's ASYNC image worker, and a frame that renders before the
            // upload lands draws nothing at all, showing the black container
            // underneath. At a 1908x1152 physical-pixel desktop that is 8.8 MB
            // per upload; asking for one on every vsync outruns the worker and
            // the misses show up as flicker. ~30/s keeps motion smooth and
            // leaves the worker room to finish.
            let due = app
                .rdp_frame_uploaded
                .is_none_or(|at| at.elapsed() >= RDP_FRAME_MIN_INTERVAL);
            if due {
                if let Some(frame) = app.manager.active_rdp_frame_if_newer(app.rdp_frame_generation) {
                    app.rdp_frame_generation = frame.generation;
                    app.rdp_frame_uploaded = Some(Instant::now());
                    app.rdp_surface_size = Some((frame.width, frame.height));
                    dump_rdp_frame(&frame);
                    app.rdp_tiles = split_into_tiles(&frame);
                }
            }
        }
        Message::RdpPointerMoved(point) => {
            // `point` already carries surface pixel coords (mapped in the view).
            app.manager.send_rdp_input_to_active(RdpInput::MouseMove {
                x: point.x.max(0.0) as u16,
                y: point.y.max(0.0) as u16,
            });
        }
        Message::RdpPressed(button) => {
            if let Some(b) = rdp_mouse_button(button) {
                app.terminal_focused = true;
                app.manager
                    .send_rdp_input_to_active(RdpInput::MouseButton { button: b, pressed: true });
            }
        }
        Message::RdpReleased(button) => {
            if let Some(b) = rdp_mouse_button(button) {
                app.manager
                    .send_rdp_input_to_active(RdpInput::MouseButton { button: b, pressed: false });
            }
        }
        Message::RdpScrolled(delta) => {
            let (vertical, amount) = match delta {
                mouse::ScrollDelta::Lines { x, y } => {
                    if y.abs() >= x.abs() {
                        (true, y)
                    } else {
                        (false, x)
                    }
                }
                mouse::ScrollDelta::Pixels { x, y } => {
                    if y.abs() >= x.abs() {
                        (true, y / 20.0)
                    } else {
                        (false, x / 20.0)
                    }
                }
            };
            if amount != 0.0 {
                // RDP wheel units: 120 per notch, sign = scroll direction.
                let units = (amount * 120.0).clamp(-32768.0, 32767.0) as i16;
                app.manager
                    .send_rdp_input_to_active(RdpInput::Wheel { vertical, delta: units });
            }
        }
        Message::ToggleMenu(menu) => {
            app.active_menu = if app.active_menu == Some(menu) {
                None
            } else {
                Some(menu)
            };
            sync_terminal_size(app);
        }
        Message::ToggleTheme => {
            // One control, three states. Ordered so the two explicit choices sit
            // next to each other and "follow the system" is the third stop
            // rather than something you pass through every time you flip.
            app.theme_mode = match app.theme_mode {
                ThemeMode::Light => ThemeMode::Dark,
                ThemeMode::Dark => ThemeMode::System,
                ThemeMode::System => ThemeMode::Light,
            };
            app.dark_mode = resolve_dark(app.theme_mode);
            app.notice = String::from(match app.theme_mode {
                ThemeMode::Light => "已切换到浅色主题",
                ThemeMode::Dark => "已切换到深色主题",
                ThemeMode::System => "已切换到跟随系统",
            });
        }
        Message::CloseAppearance => {
            app.appearance_open = false;
        }
        Message::FontFamilyChanged(index) => {
            if let Some((name, _)) = FONT_PRESETS.get(index as usize) {
                app.font_family = (*name).to_string();
            }
            // Font metrics feed the grid size, so re-fit cols/rows.
            sync_terminal_size(app);
        }
        Message::FontSizeStep(delta) => {
            step_font_size(app, delta);
        }
        Message::ModifiersChanged(modifiers) => {
            app.modifiers = modifiers;
        }
        Message::ColorSchemeChanged(index) => {
            if let Some(scheme) = COLOR_SCHEMES.get(index as usize) {
                app.color_scheme = scheme.name.to_string();
            }
        }
        Message::HostLayoutChanged(layout) => {
            app.host_layout = layout;
        }
        Message::ShowMainView(view) => {
            app.main_view = view;
            // The terminal only takes keystrokes while it is the thing on
            // screen; leaving focus behind would send typing into a hidden pane.
            app.terminal_focused = matches!(view, MainView::Terminal);
        }
        Message::OpenAppearance => {
            app.appearance_open = true;
        }
        Message::HighlightRuleToggled(id) => {
            let shipped = highlight::rules()
                .iter()
                .find(|spec| spec.id == id)
                .is_some_and(|spec| spec.enabled);
            let now_on = !app.highlight_rules.get(id).copied().unwrap_or(shipped);
            if now_on == shipped {
                // Back at the shipped value, so stop recording an opinion about
                // it — otherwise a later correction to that default would never
                // reach anyone who had ever toggled the rule twice.
                app.highlight_rules.remove(id);
            } else {
                app.highlight_rules.insert(id.to_string(), now_on);
            }
            // Rebuilds the rule set, patterns and all. Cheap here and never on
            // the render path.
            highlight::apply_overrides(&app.highlight_rules);
        }
        Message::CloseOptions => {
            app.options_open = false;
        }
        Message::CloseKnownHosts => {
            app.known_hosts_open = false;
        }
        Message::RemoveKnownHost(host, fingerprint) => {
            match remove_known_host(&known_hosts_path(), &host, &fingerprint) {
                Ok(()) => {
                    app.known_hosts = list_known_hosts(&known_hosts_path());
                    app.notice = format!("已删除受信主机密钥: {host}");
                }
                Err(error) => {
                    app.last_error = Some(format!("删除主机密钥失败: {error}"));
                }
            }
        }
        Message::PickConfigDir => {
            let mut dialog = rfd::AsyncFileDialog::new()
                .set_title("选择配置文件夹（可指向 Dropbox 等同步盘）");
            if let Some(parent) = app.config_dir.parent() {
                if parent.exists() {
                    dialog = dialog.set_directory(parent);
                }
            }
            return Task::perform(dialog.pick_folder(), |handle| {
                Message::ConfigDirPicked(handle.map(|h| h.path().to_path_buf()))
            });
        }
        Message::ConfigDirPicked(path) => {
            if let Some(path) = path {
                relocate_config_dir(app, path);
            }
        }
        Message::ResetConfigDir => {
            // Same path as picking the default folder: carry current config back
            // into the default (leaving any synced folder intact) and switch live.
            relocate_config_dir(app, adit_storage::default_config_dir());
        }
        Message::LogDirChanged(value) => {
            app.log_dir = value;
        }
        Message::LogNamePatternChanged(value) => {
            app.log_name_pattern = value;
        }
        Message::PickLogDir => {
            let mut dialog = rfd::AsyncFileDialog::new().set_title("选择日志文件夹");
            let start = effective_log_dir(app);
            let start = if start.exists() {
                start
            } else {
                app.config_dir.clone()
            };
            if start.exists() {
                dialog = dialog.set_directory(start);
            }
            return Task::perform(dialog.pick_folder(), |handle| {
                Message::LogDirPicked(handle.map(|h| h.path().to_path_buf()))
            });
        }
        Message::LogDirPicked(path) => {
            if let Some(path) = path {
                app.log_dir = path.display().to_string();
            }
        }
        Message::ToggleAutoLog(enabled) => {
            app.auto_log_on_connect = enabled;
        }
        Message::ToggleLogPlaintext(enabled) => {
            app.log_plaintext = enabled;
        }
        Message::ToggleCopyOnSelect(enabled) => {
            app.copy_on_select = enabled;
        }
        Message::ToggleRightClickPaste(enabled) => {
            app.right_click_paste = enabled;
        }
        Message::OpenConfigFolder => {
            open_folder(app, app.config_dir.clone());
        }
        Message::OpenLogFolder => {
            let dir = effective_log_dir(app);
            open_folder(app, dir);
        }
        Message::ToggleBroadcast => {
            app.broadcast_input = !app.broadcast_input;
            app.notice = if app.broadcast_input {
                String::from("输入广播已开启：键盘输入将同时发往所有已连接会话")
            } else {
                String::from("输入广播已关闭")
            };
        }
        Message::RunMenu(command) => {
            // The update check needs to return an async Task, unlike the other
            // (synchronous) menu commands.
            if matches!(command, MenuCommand::CheckUpdate) {
                return begin_update_check(app);
            }
            // The SecureCRT import opens an async folder picker.
            if matches!(command, MenuCommand::ImportSecureCrt) {
                app.active_menu = None;
                let mut dialog = rfd::AsyncFileDialog::new()
                    .set_title("选择 SecureCRT 的 Sessions 文件夹");
                if let Some(dir) = adit_storage::default_securecrt_sessions_dir() {
                    dialog = dialog.set_directory(dir);
                }
                return Task::perform(dialog.pick_folder(), |handle| {
                    Message::SecureCrtFolderPicked(handle.map(|h| h.path().to_path_buf()))
                });
            }
            run_menu_command(app, command);
            app.active_menu = None;
            sync_terminal_size(app);
        }
        Message::SecureCrtFolderPicked(path) => {
            if let Some(path) = path {
                import_securecrt(app, &path);
            }
        }
        Message::HostsCursorMoved(point) => {
            app.hosts_cursor = Some(point);
        }
        Message::GridProfilePressed(profile_id) => {
            // Same arming as ProfilePressed — selection, drag state, origin
            // point — with the origin remembered as the grid.
            select_profile(app, profile_id);
            app.dragged_profile = Some(profile_id);
            app.drag_from_grid = true;
            app.profile_drop = None;
            app.profile_drag_active = false;
            app.profile_drag_origin = Some(Point::new(
                app.cursor_pos.x,
                app.cursor_pos.y - MENU_BAR_HEIGHT - TOOLBAR_HEIGHT,
            ));
            app.group_drop_target = None;
        }
        Message::ProfilePressed(profile_id) => {
            select_profile(app, profile_id);
            app.dragged_profile = Some(profile_id);
            app.drag_from_grid = false;
            app.profile_drop = None;
            app.profile_drag_active = false;
            // Record the press point; the drag only "activates" once the pointer
            // leaves a small dead zone (cursor_pos is window-absolute; the sidebar
            // starts just below the menu bar + toolbar).
            app.profile_drag_origin = Some(Point::new(
                app.cursor_pos.x,
                app.cursor_pos.y - MENU_BAR_HEIGHT - TOOLBAR_HEIGHT,
            ));
            app.group_drop_target = None;
            app.profile_context_menu = None;
            app.group_context_menu = None;
            // Clicking another row saves any in-place rename (no confirm buttons).
            commit_inline_rename(app);
            close_profile_editor_if_other(app, profile_id);
        }
        Message::ProfileDoubleClicked(profile_id) => {
            select_profile(app, profile_id);
            app.dragged_profile = None;
            app.profile_drag_active = false;
            app.profile_drag_origin = None;
            app.profile_drop = None;
            app.group_drop_target = None;
            app.profile_context_menu = None;
            app.group_context_menu = None;
            app.profile_editor = None;
            // Double-click connects immediately, like SecureCRT/Xshell — only
            // fall back to the dialog when a password is genuinely required.
            //
            // And it leaves the host grid, which shares the tab bar with the
            // sessions: connecting from there and staying put would leave the
            // new session running behind a list of hosts.
            app.main_view = MainView::Terminal;
            connect_profile(app);
        }
        Message::ProfileHovered(profile_id) => {
            app.hovered_profile = Some(profile_id);
            // Tree rows arm tree drags only. A grid drag passing over the
            // sidebar must not retarget its drop onto a tree position — that is
            // the other half of keeping the two orderings apart.
            if !app.drag_from_grid {
                if let Some(dragged) = app.dragged_profile {
                    if dragged != profile_id {
                        app.profile_drag_active = true;
                        app.profile_drop = Some(ProfileDrop::Beside {
                            profile_id,
                            position: ProfileDropPosition::Before,
                        });
                        app.group_drop_target = None;
                    }
                }
            }
        }
        Message::ProfileHoverExited(profile_id) => {
            if app.hovered_profile == Some(profile_id) {
                app.hovered_profile = None;
            }
        }
        Message::ProfileDragOver(profile_id, position) => {
            app.hovered_profile = Some(profile_id);
            if !app.drag_from_grid {
                if let Some(dragged) = app.dragged_profile {
                    if dragged != profile_id {
                        app.profile_drag_active = true;
                        app.profile_drop = Some(ProfileDrop::Beside {
                            profile_id,
                            position,
                        });
                        app.group_drop_target = None;
                    }
                }
            }
        }
        Message::GridProfileHovered(profile_id) => {
            app.hovered_profile = Some(profile_id);
            if app.drag_from_grid {
                if let Some(dragged) = app.dragged_profile {
                    if dragged != profile_id {
                        app.profile_drag_active = true;
                        let drop = ProfileDrop::Beside {
                            profile_id,
                            position: ProfileDropPosition::Before,
                        };
                        // Only on a real change: retargeting every frame
                        // restarts the ease from wherever the card currently
                        // is, which is a card that never arrives.
                        if app.profile_drop.as_ref() != Some(&drop) {
                            app.profile_drop = Some(drop);
                            app.group_drop_target = None;
                            retarget_card_slots(app);
                        }
                    }
                }
            }
        }
        Message::GridProfileDragOver(profile_id, position) => {
            app.hovered_profile = Some(profile_id);
            if app.drag_from_grid {
                if let Some(dragged) = app.dragged_profile {
                    if dragged != profile_id {
                        app.profile_drag_active = true;
                        let drop = ProfileDrop::Beside {
                            profile_id,
                            position,
                        };
                        if app.profile_drop.as_ref() != Some(&drop) {
                            app.profile_drop = Some(drop);
                            app.group_drop_target = None;
                            retarget_card_slots(app);
                        }
                    }
                }
            }
        }
        Message::ProfileDragOverTop => {
            if app.dragged_profile.is_some() && !app.drag_from_grid {
                app.profile_drop = Some(ProfileDrop::TopLevel);
                app.group_drop_target = None;
            }
        }
        Message::ProfileDragOverBottom => {
            if app.dragged_profile.is_some() && !app.drag_from_grid {
                app.profile_drop = Some(ProfileDrop::BottomLevel);
                app.group_drop_target = None;
            }
        }
        Message::ProfileDropped(_profile_id) => {
            finish_profile_drag(app);
        }
        Message::ProfileDragOverGroup(group) => {
            if app.dragged_profile.is_some() {
                // A session dragged onto a folder header drops *into* the folder.
                app.profile_drag_active = true;
                app.group_drop_target = Some(group.clone());
                app.profile_drop = Some(ProfileDrop::IntoGroup(group));
            } else if let Some(source) = app.dragged_group.clone() {
                // A folder dragged onto another folder reorders next to it.
                if source != group {
                    app.group_drag_active = true;
                    app.group_drop = Some(group);
                } else {
                    app.group_drop = None;
                }
            }
        }
        Message::ProfileDroppedOnGroup(group) => {
            if app.dragged_group.is_some() {
                finish_group_drag_on(app, group);
            } else {
                drop_profile_on_group(app, group);
            }
        }
        Message::ProfileGroupHoverExited(group) => {
            if app.dragged_profile.is_none()
                && app.group_drop_target.as_deref() == Some(group.as_str())
            {
                app.group_drop_target = None;
            }
            if app.dragged_group.is_some() && app.group_drop.as_deref() == Some(group.as_str()) {
                app.group_drop = None;
            }
        }
        Message::CancelProfileDrag => {
            finish_profile_drag(app);
            // A folder released off any header (over empty space or a session row)
            // still commits its reorder from the last-hovered target.
            cancel_group_drag(app);
            app.sftp_drag_cursor = None;
            // A left-button release also resolves a pane-to-pane SFTP drag:
            // transfer only if the pointer ended over the *other* pane.
            if let Some((src, name)) = app.sftp_drag.take() {
                if let Some(dst) = app.sftp_drag_over.take() {
                    if dst != src {
                        let selection = match src {
                            SftpPane::Local => &app.sftp_local_selected,
                            SftpPane::Remote => &app.sftp_remote_selected,
                        };
                        let names: Vec<String> = if selection.contains(&name) && selection.len() > 1
                        {
                            selection.iter().cloned().collect()
                        } else {
                            vec![name]
                        };
                        for entry in names {
                            match src {
                                SftpPane::Local => app.manager.sftp_upload_local(&entry),
                                SftpPane::Remote => app.manager.sftp_download(&entry),
                            }
                        }
                    }
                }
            }
        }
        Message::ShowGroupContextMenu(group) => {
            // Anchor the floating menu at the cursor (last tracked position).
            app.context_menu_pos = app.cursor_pos;
            app.group_context_menu = Some(group);
            app.profile_context_menu = None;
            app.profile_editor = None;
            app.terminal_context_menu = false;
            commit_inline_rename(app);
        }
        Message::HideGroupContextMenu => {
            app.group_context_menu = None;
        }
        Message::RenameGroupFromContext(group) => {
            // Save any other in-place rename first, then start this one.
            commit_inline_rename(app);
            // Blur the terminal so keys the rename input ignores don't leak to the
            // active session (the session-rename path gets this via select_profile).
            app.terminal_focused = false;
            app.group_context_menu = None;
            app.editing_group = Some(group.clone());
            app.group_name_draft = group;
            return focus_rename_input();
        }
        Message::NewProfileInGroup(group) => {
            app.group_context_menu = None;
            app.profile_group = group;
            new_profile_draft(app);
        }
        Message::DeleteGroupFromContext(group) => {
            delete_group(app, group);
        }
        Message::GroupNameDraftChanged(value) => {
            app.group_name_draft = value;
        }
        Message::SaveGroupRename => {
            save_group_rename(app);
        }
        Message::RenameProfileFromContext(profile_id) => {
            // Save any other in-place rename first, then start this one.
            commit_inline_rename(app);
            select_profile(app, profile_id);
            app.profile_context_menu = None;
            let current = app
                .manager
                .profile(profile_id)
                .map(|profile| profile.name.clone())
                .unwrap_or_default();
            app.editing_profile = Some(profile_id);
            app.profile_name_draft = current;
            return focus_rename_input();
        }
        Message::ProfileNameDraftChanged(value) => {
            app.profile_name_draft = value;
        }
        Message::SaveProfileRename => {
            save_profile_rename(app);
        }
        Message::ShowProfileContextMenu(profile_id) => {
            select_profile(app, profile_id);
            app.dragged_profile = None;
            app.group_drop_target = None;
            // Anchor the floating menu at the cursor (last tracked position).
            app.context_menu_pos = app.cursor_pos;
            app.profile_context_menu = Some(profile_id);
            app.group_context_menu = None;
            app.terminal_context_menu = false;
            commit_inline_rename(app);
        }
        Message::HideProfileContextMenu => {
            app.profile_context_menu = None;
        }
        Message::GlobalCursorMoved(point) => {
            // Already window-absolute; keeps the anchor fresh over widgets (like
            // the tab strip) that don't report their own cursor moves.
            app.cursor_pos = point;
        }
        Message::SidebarCursorMoved(point) => {
            // `point` is sidebar-relative; the context-menu anchor wants it in
            // window-absolute coordinates.
            app.cursor_pos = Point::new(point.x, point.y + MENU_BAR_HEIGHT + TOOLBAR_HEIGHT);
            if app.dragged_profile.is_some() {
                // Promote to a real drag once the pointer leaves a small dead zone
                // around the press point — a click/double-click stays inside it.
                if let Some(origin) = app.profile_drag_origin {
                    if point.distance(origin) > 5.0 {
                        app.profile_drag_active = true;
                    }
                }
            }
            if app.dragged_group.is_some() {
                // Same dead-zone promotion for a folder drag, so a plain folder
                // click still toggles collapse instead of reordering.
                if let Some(origin) = app.group_drag_origin {
                    if point.distance(origin) > 5.0 {
                        app.group_drag_active = true;
                    }
                }
            }
        }
        Message::EditProfileFromContext(profile_id) => {
            select_profile(app, profile_id);
            app.profile_context_menu = None;
            app.profile_editor = Some(profile_id);
            app.notice = String::from("已打开会话编辑面板");
        }
        Message::CloseProfileEditor => {
            app.profile_editor = None;
        }
        Message::ConnectProfileFromContext(profile_id) => {
            select_profile(app, profile_id);
            app.profile_context_menu = None;
            app.profile_editor = None;
            open_connection_dialog(app);
        }
        Message::CloneProfileFromContext(profile_id) => {
            app.profile_context_menu = None;
            if let Some(new_id) = app.manager.duplicate_profile(profile_id) {
                // Copy the source's saved password + key passphrase (kept in the
                // OS vault under the profile id) to the clone so its auth works.
                if let Ok(Some(password)) = app.credential_store.load_profile_password(profile_id) {
                    let _ = app.credential_store.save_profile_password(new_id, &password);
                }
                if let Ok(Some(passphrase)) =
                    app.credential_store.load_profile_passphrase(profile_id)
                {
                    let _ = app
                        .credential_store
                        .save_profile_passphrase(new_id, &passphrase);
                }
                select_profile(app, new_id);
                if persist_profiles(app) {
                    app.notice = String::from("已克隆会话");
                }
            }
        }
        Message::DeleteProfileFromContext(profile_id) => {
            select_profile(app, profile_id);
            app.profile_context_menu = None;
            delete_selected_profile(app);
        }
        Message::ConnectionPasswordChanged(password) => {
            app.password = password;
        }
        Message::RememberConnectionPasswordChanged(remember) => {
            app.remember_connection_password = remember;
        }
        Message::ConfirmConnection => {
            confirm_connection(app);
        }
        Message::CancelConnection => {
            app.connection_dialog = None;
            app.password.clear();
            app.remember_connection_password = false;
        }
        Message::RespondHostKey { session_id, accept } => {
            if let Err(error) = app.manager.respond_host_key(session_id, accept) {
                app.last_error = Some(error.to_string());
            } else {
                app.notice = if accept {
                    String::from("已信任主机密钥，继续连接")
                } else {
                    String::from("已拒绝主机密钥")
                };
            }
        }
        Message::AuthPromptInput { index, value } => {
            if let Some(slot) = app.auth_prompt_answers.get_mut(index) {
                *slot = value;
            }
        }
        Message::SubmitAuthPrompt { session_id } => {
            let answers = std::mem::take(&mut app.auth_prompt_answers);
            app.auth_prompt = None;
            if let Err(error) = app.manager.respond_auth_prompt(session_id, answers) {
                app.last_error = Some(error.to_string());
            }
        }
        Message::CancelAuthPrompt { session_id } => {
            app.auth_prompt_answers.clear();
            app.auth_prompt = None;
            // An empty answer set cancels the authentication.
            if let Err(error) = app.manager.respond_auth_prompt(session_id, Vec::new()) {
                app.last_error = Some(error.to_string());
            }
        }
        Message::OpenHyperlink(url) => {
            // Terminal output is remote-controlled: confirm the real destination
            // before opening, and only offer http(s).
            if is_openable_http_url(&url) {
                app.pending_hyperlink = Some(url);
            } else {
                app.last_error = Some(String::from("仅支持打开 http/https 链接"));
            }
        }
        Message::ConfirmOpenHyperlink => {
            if let Some(url) = app.pending_hyperlink.take() {
                open_external_link(app, &url);
            }
        }
        Message::CancelOpenHyperlink => {
            app.pending_hyperlink = None;
        }
        Message::CloseSftp => {
            app.manager.close_sftp();
            app.sftp_rename = None;
            app.sftp_delete_target = None;
            app.sftp_context_menu = None;
            app.sftp_new_folder.clear();
            app.sftp_drag = None;
            app.sftp_drag_over = None;
            app.sftp_drag_cursor = None;
            app.sftp_local_selected.clear();
            app.sftp_remote_selected.clear();
            app.sftp_local_path_edit.clear();
            app.sftp_remote_path_edit.clear();
            app.sftp_local_cwd_seen.clear();
            app.sftp_remote_cwd_seen.clear();
            app.sftp_last_click = None;
        }
        Message::OpenTunnels => {
            if app.manager.active_session().is_none() {
                app.last_error = Some(String::from("请先连接一个会话再配置端口转发"));
            } else {
                app.tunnels_open = true;
                app.last_error = None;
            }
        }
        Message::CloseTunnels => app.tunnels_open = false,
        Message::CloseAbout => app.about_open = false,
        Message::TunnelKindChanged(kind) => app.tunnel_kind = kind,
        Message::TunnelBindAddrChanged(value) => app.tunnel_bind_addr = value,
        Message::TunnelBindPortChanged(value) => {
            app.tunnel_bind_port = value.chars().filter(char::is_ascii_digit).collect();
        }
        Message::TunnelTargetHostChanged(value) => app.tunnel_target_host = value,
        Message::TunnelTargetPortChanged(value) => {
            app.tunnel_target_port = value.chars().filter(char::is_ascii_digit).collect();
        }
        Message::ToggleTunnelSave(value) => app.tunnel_save = value,
        Message::AddTunnel => add_tunnel(app),
        Message::CloseTunnel(id) => app.manager.close_tunnel(id),
        Message::RemoveSavedTunnel(index) => {
            if let Some(profile_id) = app.manager.active_session_summary().map(|s| s.profile_id) {
                app.manager.remove_profile_tunnel(profile_id, index);
                persist_profiles(app);
            }
        }
        Message::SftpNavigate(name) => {
            app.sftp_context_menu = None;
            app.manager.sftp_navigate(&name);
        }
        Message::SftpUp => app.manager.sftp_up(),
        Message::SftpRefresh => app.manager.sftp_refresh(),
        Message::SftpLocalNavigate(name) => {
            app.sftp_context_menu = None;
            app.manager.sftp_local_navigate(&name);
        }
        Message::SftpLocalUp => app.manager.sftp_local_up(),
        Message::SftpLocalRefresh => app.manager.sftp_local_refresh(),
        Message::SftpUploadLocal(name) => {
            app.sftp_context_menu = None;
            app.manager.sftp_upload_local(&name);
        }
        Message::SftpDownload(name) => {
            app.sftp_context_menu = None;
            app.manager.sftp_download(&name);
        }
        Message::SftpCursorMoved(point) => {
            // The SFTP panel is a full-window overlay, so on_move gives window
            // coordinates directly — used to anchor the right-click menu.
            app.cursor_pos = point;
        }
        Message::ShowSftpContextMenu(pane, name, is_dir) => {
            app.context_menu_pos = app.cursor_pos;
            app.sftp_context_menu = Some((pane, name, is_dir));
            app.profile_context_menu = None;
            app.group_context_menu = None;
            app.terminal_context_menu = false;
        }
        Message::HideSftpContextMenu => {
            app.sftp_context_menu = None;
        }
        Message::SftpRowPress(pane, name) => {
            // Arm a potential pane-to-pane drag; it only fires if the pointer is
            // released over the other pane (see PointerReleased).
            app.sftp_drag = Some((pane, name.clone()));
            app.sftp_drag_over = Some(pane);
            let now = Instant::now();
            let is_double = matches!(
                &app.sftp_last_click,
                Some((p, n, t)) if *p == pane && *n == name && now.duration_since(*t) < Duration::from_millis(450)
            );
            if is_double {
                // Double-click transfers just this file (selection untouched).
                app.sftp_last_click = None;
                match pane {
                    SftpPane::Remote => app.manager.sftp_download(&name),
                    SftpPane::Local => app.manager.sftp_upload_local(&name),
                }
            } else {
                // Single click toggles the file in the pane's selection.
                app.sftp_last_click = Some((pane, name.clone(), now));
                let set = match pane {
                    SftpPane::Remote => &mut app.sftp_remote_selected,
                    SftpPane::Local => &mut app.sftp_local_selected,
                };
                if !set.remove(&name) {
                    set.insert(name);
                }
            }
        }
        Message::SftpTransferSelected(pane) => match pane {
            SftpPane::Remote => {
                for name in std::mem::take(&mut app.sftp_remote_selected) {
                    app.manager.sftp_download(&name);
                }
            }
            SftpPane::Local => {
                for name in std::mem::take(&mut app.sftp_local_selected) {
                    app.manager.sftp_upload_local(&name);
                }
            }
        },
        Message::SftpFileDropped(path) => {
            if path.is_dir() {
                app.last_error = Some(String::from("暂不支持上传文件夹，请拖入单个文件"));
            } else if app.manager.active_is_sftp_shell() {
                // Dropped onto a command-line SFTP tab: upload into its cwd.
                if let Err(error) = app.manager.sftp_shell_upload_dropped(&path) {
                    app.last_error = Some(error.to_string());
                }
            } else if !app.manager.sftp_is_open() {
                app.notice =
                    String::from("拖拽上传：请先打开 SFTP (Alt+P 开命令行，或打开 SFTP 面板)");
            } else if let Err(error) = app.manager.sftp_upload(&path) {
                app.last_error = Some(error.to_string());
            } else {
                app.notice = format!("上传 {}", path.display());
            }
        }
        Message::SftpLocalPathChanged(value) => app.sftp_local_path_edit = value,
        Message::SftpLocalGo => app
            .manager
            .sftp_local_goto(std::path::Path::new(&app.sftp_local_path_edit)),
        Message::SftpRemotePathChanged(value) => app.sftp_remote_path_edit = value,
        Message::SftpRemoteGo => app.manager.sftp_goto(&app.sftp_remote_path_edit),
        Message::SftpUploadPathChanged(value) => app.sftp_upload_path = value,
        Message::SftpUpload => {
            let path = app.sftp_upload_path.trim().to_string();
            if path.is_empty() {
                app.last_error = Some(String::from("请输入要上传的本地文件路径"));
            } else {
                match app.manager.sftp_upload(std::path::Path::new(&path)) {
                    Ok(()) => {
                        app.sftp_upload_path.clear();
                        app.last_error = None;
                    }
                    Err(error) => app.last_error = Some(error.to_string()),
                }
            }
        }
        Message::SftpPickUpload => {
            return Task::perform(
                rfd::AsyncFileDialog::new()
                    .set_title("选择要上传的文件")
                    .pick_file(),
                |handle| Message::SftpUploadPicked(handle.map(|h| h.path().to_path_buf())),
            );
        }
        Message::SftpUploadPicked(path) => {
            if let Some(path) = path {
                if let Err(error) = app.manager.sftp_upload(&path) {
                    app.last_error = Some(error.to_string());
                }
            }
        }
        Message::SftpNewFolderChanged(value) => app.sftp_new_folder = value,
        Message::SftpMkdir => {
            let name = app.sftp_new_folder.trim().to_string();
            if !name.is_empty() {
                app.manager.sftp_mkdir(&name);
                app.sftp_new_folder.clear();
            }
        }
        Message::SftpBeginRename(pane, name) => {
            app.sftp_context_menu = None;
            app.sftp_rename_to = name.clone();
            app.sftp_rename = Some((pane, name));
            app.sftp_delete_target = None;
        }
        Message::SftpRenameToChanged(value) => app.sftp_rename_to = value,
        Message::SftpConfirmRename => {
            if let Some((pane, from)) = app.sftp_rename.take() {
                let to = app.sftp_rename_to.trim().to_string();
                if !to.is_empty() && to != from {
                    match pane {
                        SftpPane::Remote => app.manager.sftp_rename(&from, &to),
                        SftpPane::Local => app.manager.sftp_local_rename(&from, &to),
                    }
                }
            }
            app.sftp_rename_to.clear();
        }
        Message::SftpCancelRename => {
            app.sftp_rename = None;
            app.sftp_rename_to.clear();
        }
        Message::SftpBeginDelete(pane, name, is_dir) => {
            app.sftp_context_menu = None;
            app.sftp_delete_target = Some((pane, name, is_dir));
            app.sftp_rename = None;
        }
        Message::SftpConfirmDelete => {
            if let Some((pane, name, is_dir)) = app.sftp_delete_target.take() {
                match pane {
                    SftpPane::Remote => app.manager.sftp_delete(&name, is_dir),
                    SftpPane::Local => app.manager.sftp_local_delete(&name, is_dir),
                }
            }
        }
        Message::SftpCancelDelete => app.sftp_delete_target = None,
        Message::SftpSort(pane, key) => {
            let slot = match pane {
                SftpPane::Local => &mut app.sftp_local_sort,
                SftpPane::Remote => &mut app.sftp_remote_sort,
            };
            // Toggle direction when re-selecting the same column; else default ascending.
            if slot.0 == key {
                slot.1 = !slot.1;
            } else {
                *slot = (key, true);
            }
        }
        Message::SftpClearTransfers => app.manager.sftp_clear_finished(),
        Message::SftpCancelTransfer(id) => app.manager.sftp_cancel_transfer(id),
        Message::SftpCancelAll => app.manager.sftp_cancel_all(),
        Message::SftpDragEnter(pane) => app.sftp_drag_over = Some(pane),
        Message::SftpDragMove(pane, position) => {
            if app.sftp_drag.is_some() {
                app.sftp_drag_over = Some(pane);
                app.sftp_drag_cursor = Some(position);
            }
        }
        Message::ToggleProfileGroup(group) => {
            toggle_group_collapsed(app, &group);
        }
        Message::GroupPressed(group) => {
            // Arm a folder drag. It only turns into a real reorder once the pointer
            // leaves a small dead zone (see SidebarCursorMoved); a plain click
            // releases still inside it and falls back to toggling collapse.
            app.dragged_group = Some(group);
            app.group_drag_active = false;
            app.group_drop = None;
            app.group_drag_origin = Some(Point::new(
                app.cursor_pos.x,
                app.cursor_pos.y - MENU_BAR_HEIGHT - TOOLBAR_HEIGHT,
            ));
            // A folder press cancels any in-flight session drag / menus and saves
            // any in-place rename (clicking away commits — no confirm buttons).
            app.dragged_profile = None;
            app.profile_drag_active = false;
            app.profile_drop = None;
            app.profile_context_menu = None;
            app.group_context_menu = None;
            commit_inline_rename(app);
        }
        Message::ProfileGroupChanged(value) => {
            app.terminal_focused = false;
            app.profile_group = value;
        }
        Message::ProfileNameChanged(value) => {
            app.terminal_focused = false;
            app.profile_name = value;
        }
        Message::ProfileHostChanged(value) => {
            app.terminal_focused = false;
            app.profile_host = value;
        }
        Message::ProfilePortChanged(value) => {
            app.terminal_focused = false;
            app.profile_port = value;
        }
        Message::ProfileUsernameChanged(value) => {
            app.terminal_focused = false;
            app.profile_username = value;
        }
        Message::ProfileAuthMethodChanged(auth_method) => {
            app.terminal_focused = false;
            app.profile_auth_method = auth_method;
        }
        Message::ProfilePasswordChanged(value) => {
            app.terminal_focused = false;
            app.profile_password = value;
        }
        Message::ProfilePassphraseChanged(value) => {
            app.terminal_focused = false;
            app.profile_passphrase = value;
        }
        Message::ProfileIconChanged(icon) => {
            app.terminal_focused = false;
            app.profile_icon = icon.to_string();
        }
        Message::ProfileProtocolChanged(protocol) => {
            app.terminal_focused = false;
            // Nudge the port to a sensible default when moving to/from RDP.
            let port = app.profile_port.trim();
            if protocol == Protocol::Rdp && (port.is_empty() || port == "22") {
                app.profile_port = String::from("3389");
            } else if protocol == Protocol::Ssh && port == "3389" {
                app.profile_port = String::from("22");
            }
            // SSH defaults to password auth; only upgrade the implicit "Auto" so an
            // explicit Key/Agent choice is preserved.
            if protocol == Protocol::Ssh && app.profile_auth_method == AuthMethod::Auto {
                app.profile_auth_method = AuthMethod::Password;
            }
            app.profile_protocol = protocol;
        }
        Message::ProfileIdentityFileChanged(value) => {
            app.terminal_focused = false;
            app.profile_identity_file = value;
        }
        Message::PickIdentityFile => {
            let start = adit_storage::home_dir().map(|home| home.join(".ssh"));
            let mut dialog = rfd::AsyncFileDialog::new().set_title("选择 SSH 私钥文件");
            if let Some(dir) = start.filter(|dir| dir.exists()) {
                dialog = dialog.set_directory(dir);
            }
            return Task::perform(dialog.pick_file(), |handle| {
                Message::IdentityFilePicked(handle.map(|h| h.path().to_path_buf()))
            });
        }
        Message::IdentityFilePicked(path) => {
            if let Some(path) = path {
                app.profile_identity_file = path.display().to_string();
            }
        }
        Message::ProfileStartupCommandChanged(value) => {
            app.terminal_focused = false;
            app.profile_startup_command = value;
        }
        Message::ProfileJumpsChanged(value) => {
            app.terminal_focused = false;
            app.profile_jumps = value;
        }
        Message::ProfileEnvironmentChanged(environment) => {
            app.terminal_focused = false;
            app.profile_environment = environment;
            // Prefill a sensible custom colour so the picker isn't blank.
            if environment == Environment::Custom && app.profile_accent_color.trim().is_empty() {
                app.profile_accent_color = String::from("#3f7fd1");
            }
        }
        Message::ProfileAccentColorChanged(value) => {
            app.terminal_focused = false;
            app.profile_accent_color = value;
        }
        Message::ProfileLabelChanged(value) => {
            app.terminal_focused = false;
            app.profile_label = value;
        }
        Message::ProfileTerminalTypeChanged(value) => {
            app.terminal_focused = false;
            app.profile_terminal_type = value;
        }
        Message::ConnectTimeoutChanged(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                app.connect_timeout_secs = 0;
            } else if let Ok(secs) = trimmed.parse::<u32>() {
                app.connect_timeout_secs = secs.min(600);
            }
            app.manager
                .set_connect_timeout(u64::from(app.connect_timeout_secs));
        }
        Message::ScrollbackLinesChanged(value) => {
            let trimmed = value.trim();
            if let Ok(lines) = trimmed.parse::<u32>() {
                app.scrollback_lines = lines.clamp(200, 200_000);
                adit_terminal::set_scrollback_limit(app.scrollback_lines as usize);
            } else if trimmed.is_empty() {
                app.scrollback_lines = 0;
            }
        }
        Message::SessionFilterChanged(value) => {
            app.terminal_focused = false;
            app.session_filter = value;
        }
        Message::NewProfileDraft => {
            new_profile_draft(app);
        }
        Message::NewGroupDraft => {
            new_group_draft(app);
        }
        Message::SaveProfile => {
            // A successful save closes the editor dialog (no-op when it is not open).
            if save_profile_from_form(app, true).is_some() {
                app.profile_editor = None;
            }
        }
        Message::DeleteSelectedProfile => {
            delete_selected_profile(app);
        }
        Message::TerminalInputChanged(input) => {
            app.terminal_focused = false;
            app.command_history_pos = None;
            // "Send characters immediately": forward the typed delta to the
            // target as it changes, so a broadcast types live on every host.
            if app.command_send_immediately {
                if let Some(bytes) = command_input_delta(&app.terminal_input, &input) {
                    app.terminal_input = input;
                    send_command_bytes(app, bytes);
                    return Task::none();
                }
            }
            app.terminal_input = input;
        }
        Message::CloseSyncPanel => {
            app.sync_open = false;
            app.sync_secret_draft.clear();
            persist_settings_if_changed(app);
        }
        Message::SyncProviderChanged(provider) => {
            app.sync.provider = provider;
            // The draft belongs to whichever provider was on screen; carrying
            // it across would offer a GitHub token as an S3 secret key.
            app.sync_secret_draft.clear();
            app.sync_secret_saved = sync_secret_name(provider)
                .and_then(|name| app.credential_store.load_secret(name).ok().flatten())
                .is_some_and(|secret| !secret.is_empty());
            app.sync_status.clear();
            app.sync_conflicts.clear();
            persist_settings_if_changed(app);
        }
        Message::SyncFieldChanged(field, value) => {
            match field {
                SyncField::GistId => app.sync.gist_id = value,
                SyncField::WebDavUrl => app.sync.webdav_url = value,
                SyncField::WebDavUsername => app.sync.webdav_username = value,
                SyncField::S3Endpoint => app.sync.s3_endpoint = value,
                SyncField::S3Region => app.sync.s3_region = value,
                SyncField::S3Bucket => app.sync.s3_bucket = value,
                SyncField::S3Key => app.sync.s3_key = value,
                SyncField::S3AccessKey => app.sync.s3_access_key = value,
                SyncField::GoogleClientId => app.sync.google_client_id = value,
                SyncField::OneDriveClientId => app.sync.onedrive_client_id = value,
                SyncField::DropboxClientId => app.sync.dropbox_client_id = value,
                SyncField::GoogleClientSecret => app.sync.google_client_secret = value,
            }
            persist_settings_if_changed(app);
        }
        Message::SyncSecretChanged(value) => {
            app.sync_secret_draft = value;
        }
        Message::SyncIncludeCredentialsToggled(enabled) => {
            app.sync.include_credentials = enabled;
            persist_settings_if_changed(app);
        }
        Message::SyncConnectAccount => {
            if app.sync_connecting {
                return Task::none();
            }
            let Some(config) = sync_oauth_config(app, app.sync.provider) else {
                app.sync_status =
                    String::from("此构建没有该云服务的 client id，可在下方填写自己的");
                return Task::none();
            };
            match adit_sync::backend::oauth::begin(config) {
                Ok(pending) => {
                    // Open the browser from here rather than inside the worker:
                    // the listener is already bound, so the redirect cannot
                    // arrive before anything is waiting for it.
                    let url = pending.url.clone();
                    open_url(app, &url);
                    app.sync_connecting = true;
                    app.sync_status = String::from("已在浏览器中打开授权页，完成后自动返回…");
                    return Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || {
                                pending
                                    .complete()
                                    .map(|tokens| tokens.refresh_token.unwrap_or_default())
                                    .map_err(|error| error.to_string())
                            })
                            .await
                            .unwrap_or_else(|error| Err(error.to_string()))
                        },
                        Message::SyncAuthFinished,
                    );
                }
                Err(error) => app.sync_status = format!("无法开始授权: {error}"),
            }
        }
        Message::SyncAuthFinished(result) => {
            app.sync_connecting = false;
            match result {
                Ok(token) if !token.trim().is_empty() => {
                    let Some(name) = sync_secret_name(app.sync.provider) else {
                        return Task::none();
                    };
                    match app.credential_store.save_secret(name, &token) {
                        Ok(()) => {
                            app.sync_secret_saved = true;
                            app.sync_status = String::from("账号已连接，可以同步了");
                        }
                        Err(error) => app.sync_status = format!("保存令牌失败: {error}"),
                    }
                }
                // Authorised, but with nothing that survives a restart. Worth
                // naming precisely: it means the consent did not include
                // offline access, and syncing would work until the access
                // token expired and then stop for no visible reason.
                Ok(_) => {
                    app.sync_status =
                        String::from("授权成功但未返回长期令牌，请在授权页确认已允许离线访问");
                }
                Err(error) => app.sync_status = format!("授权失败: {error}"),
            }
        }
        Message::SyncNow => {
            if app.sync_busy {
                return Task::none();
            }
            // A typed secret is committed here rather than on every keystroke,
            // so a half-typed token never replaces a working one.
            if !app.sync_secret_draft.is_empty() {
                if let Some(name) = sync_secret_name(app.sync.provider) {
                    if let Err(error) = app
                        .credential_store
                        .save_secret(name, &app.sync_secret_draft)
                    {
                        app.sync_status = format!("保存密钥失败: {error}");
                        return Task::none();
                    }
                    app.sync_secret_saved = true;
                    app.sync_secret_draft.clear();
                }
            }
            let Some(mut backend) = build_sync_backend(app) else {
                app.sync_status = String::from("请先填写该云服务所需的信息");
                return Task::none();
            };

            app.sync_busy = true;
            app.sync_status = String::from("正在同步…");
            app.sync_conflicts.clear();

            let store =
                adit_sync::orchestrate::SyncStateStore::new(app.config_dir.join("sync-state.json"));
            let catalog = catalog_snapshot(app);
            // An unreadable credential file must not pass for "the user did
            // not ask for credentials". Dropping it silently reads as a clean
            // sync and only surfaces much later, on the machine that then
            // cannot sign in to anything.
            let credentials = if app.sync.include_credentials {
                match std::fs::read(app.credential_store.path()) {
                    Ok(bytes) => Some(hex::encode(bytes)),
                    // Nothing saved on this machine yet — nothing to send, and
                    // nothing wrong.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => {
                        app.sync_busy = false;
                        app.sync_status = format!("读取本机密码库失败，已中止同步: {error}");
                        return Task::none();
                    }
                }
            } else {
                None
            };
            let extras = adit_sync::orchestrate::Extras {
                // The sync section stays home: it is per-machine, and it holds
                // account identifiers that do not belong in a document the
                // user is invited to read.
                settings: Some(current_settings(app).without_sync_config()),
                credentials,
            };
            let device = hostname_or_unknown();
            let now = rfc3339_now();

            // Blocking HTTP on a worker thread: the UI thread must not wait on
            // a network round trip, and iced's runtime is what draws with it.
            return Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        let result = adit_sync::orchestrate::sync(
                            backend.as_mut(),
                            &store,
                            &catalog,
                            &extras,
                            &device,
                            &now,
                        );
                        // Read the id AFTER the sync: a first push is what
                        // mints it, and the backend is dropped with this
                        // closure, so this is the only chance to keep it.
                        let assigned_id = backend.assigned_id();
                        result
                        .map(|outcome| SyncReport {
                            conflicts: outcome
                                .conflicts
                                .iter()
                                .map(|conflict| format!("冲突（已保留本机）: {}", conflict.name))
                                .collect(),
                            summary: if outcome.uploaded {
                                format!(
                                    "同步完成：新增 {} 项，更新 {} 项，删除 {} 项",
                                    outcome.stats.added_from_remote,
                                    outcome.stats.updated_from_remote,
                                    outcome.stats.deleted
                                )
                            } else {
                                String::from("已是最新，无需上传")
                            },
                            catalog: outcome.catalog,
                            assigned_id,
                        })
                        .map_err(|error| error.to_string())
                    })
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
                },
                Message::SyncFinished,
            );
        }
        Message::SyncFinished(result) => {
            app.sync_busy = false;
            match result {
                Ok(report) => {
                    app.sync_status = report.summary;
                    app.sync_conflicts = report.conflicts;
                    if let Some(id) = report.assigned_id {
                        app.sync.gist_id = id;
                    }
                    // Adopt what the merge produced, then persist it the same
                    // way any other profile change is persisted.
                    app.groups = report.catalog.groups.clone();
                    app.manager.replace_profiles(report.catalog.profiles.clone());
                    persist_profiles(app);
                    persist_settings_if_changed(app);
                }
                Err(error) => app.sync_status = format!("同步失败: {error}"),
            }
        }
        Message::ToggleFullscreen => {
            app.fullscreen = !app.fullscreen;
            let mode = if app.fullscreen {
                window::Mode::Fullscreen
            } else {
                window::Mode::Windowed
            };
            app.notice = String::from(if app.fullscreen {
                "已进入全屏 — Ctrl+Alt+Enter 退出"
            } else {
                "已退出全屏"
            });
            // Deliberately NOT re-syncing the viewport here: the window has
            // not resized yet, so this would measure the old geometry against
            // the new chrome and ship a wrong desktop size. Changing the mode
            // always produces a `WindowResized`, and that handler does it with
            // the real numbers.
            return window::latest().and_then(move |id| window::set_mode(id, mode));
        }
        Message::KeyboardInput(event) => {
            // Fullscreen toggle first, ahead of every forwarding path: RDP
            // sends unmatched keys straight to the remote desktop, so a
            // shortcut checked later would never be seen locally. Ctrl+Alt+Enter
            // is the mstsc convention, and unlike F11 it is not a key remote
            // applications expect to receive.
            if let keyboard::Event::KeyPressed { key, modifiers, .. } = &event {
                if modifiers.control()
                    && modifiers.alt()
                    && matches!(key, keyboard::Key::Named(keyboard::key::Named::Enter))
                {
                    return Task::done(Message::ToggleFullscreen);
                }
            }
            // Alt+R / Alt+I jump to the toolbar's host box and the sidebar filter —
            // the two shortcuts those placeholders advertise. Both take focus away
            // from the terminal so the typed text lands in the box, not the session.
            if alt_shortcut(&event, 'i') {
                // Filtering is pointless with the sidebar hidden; reveal it first.
                if !app.sidebar_visible {
                    app.sidebar_visible = true;
                    sync_terminal_size(app);
                }
                app.terminal_focused = false;
                return focus_session_filter();
            }
            // Alt+P opens a command-line SFTP tab for the active session
            // (SecureCRT-style), regardless of focus. This is the `sftp>` prompt,
            // not the dual-pane panel — that one has its own toolbar button.
            if alt_shortcut(&event, 'p') {
                match app.manager.open_sftp_shell_for_active() {
                    Ok(_) => {
                        app.terminal_focused = true;
                        app.last_error = None;
                        app.notice = String::from("已打开 SFTP 命令行 (输入 help 查看命令)");
                        sync_terminal_size(app);
                    }
                    Err(error) => {
                        app.last_error = Some(format!("打开 SFTP 失败: {error}"));
                    }
                }
                return Task::none();
            }
            // Ctrl+Shift+F opens scrollback search regardless of focus; Escape
            // closes it. These run before the terminal-focus gate.
            if terminal_shortcut(&event, 'f') {
                app.search_open = true;
                app.terminal_focused = false;
                recompute_search(app);
                return focus_search_input();
            }
            if app.search_open && is_escape_key(&event) {
                app.search_open = false;
                app.search_matches.clear();
                app.search_index = None;
                app.terminal_focused = true;
                return Task::none();
            }
            // Escape cancels an in-place rename (the focused text input ignores
            // Escape, so the key reaches us here).
            if is_escape_key(&event) && (app.editing_profile.is_some() || app.editing_group.is_some())
            {
                app.editing_profile = None;
                app.profile_name_draft.clear();
                app.editing_group = None;
                app.group_name_draft.clear();
                return Task::none();
            }

            // RDP: keys go to the remote desktop as scancodes, not VT bytes —
            // including Ctrl+C/Ctrl+V, which the remote handles itself. The two
            // clipboards are kept in sync out of band, on the Tick above.
            //
            // Deliberately ABOVE the `terminal_focused` gate: mouse input to
            // the surface was never gated on that flag, and half the UI clears
            // it (every dialog field, the filter box, the hosts view), so keys
            // silently died while clicks kept landing — "can click the logon
            // screen but can't type into it". A focused local text field still
            // wins: its captured events never reach this handler at all.
            if app.manager.active_is_rdp() && app.main_view == MainView::Terminal {
                // A dead session: Enter reconnects (SecureCRT-style) rather
                // than firing a scancode at a closed helper.
                if is_enter_key(&event) && active_session_is_dead(app) {
                    reconnect_active_session(app);
                    return Task::none();
                }
                if let Some((scancode, extended, pressed)) = encode_rdp_scancode(&event) {
                    app.manager.send_rdp_input_to_active(RdpInput::Key {
                        scancode,
                        extended,
                        pressed,
                    });
                } else if let keyboard::Event::KeyPressed { text: Some(text), .. } = &event {
                    // Unmapped physical key (remapped layouts, some IME setups
                    // report Unidentified): fall back to the Unicode input
                    // path instead of dropping the keystroke on the floor.
                    for ch in text.chars() {
                        app.manager
                            .send_rdp_input_to_active(RdpInput::Unicode { ch, pressed: true });
                        app.manager
                            .send_rdp_input_to_active(RdpInput::Unicode { ch, pressed: false });
                    }
                }
                return Task::none();
            }

            if !app.terminal_focused {
                return Task::none();
            }

            // A dead session: Enter reconnects (SecureCRT-style) instead of going
            // nowhere.
            if is_enter_key(&event) && active_session_is_dead(app) {
                reconnect_active_session(app);
                return Task::none();
            }

            if is_terminal_copy_shortcut(&event) {
                let text = selected_terminal_text(app);
                if !text.is_empty() {
                    app.notice = if app.terminal_selection.is_some() {
                        String::from("已复制终端选区")
                    } else {
                        String::from("已复制当前终端可见文本")
                    };
                    return clipboard::write(text);
                }
                return Task::none();
            }

            if is_terminal_paste_shortcut(&event) {
                return clipboard::read().map(Message::ClipboardPasted);
            }

            if let Some(action) = terminal_scroll_shortcut(&event, app.terminal_size.rows) {
                apply_terminal_scroll(app, action);
                return Task::none();
            }

            if let Some(bytes) = encode_keyboard_event(event) {
                send_terminal_bytes(app, bytes);
            }
        }
        Message::WindowResized { width, height, window } => {
            // Minimizing reports a 0x0 size on Windows; ignore it so we never
            // persist (and later restore) an invisible window.
            if width >= MIN_WINDOW_DIM && height >= MIN_WINDOW_DIM {
                app.window_width = width;
                app.window_height = height;
                sync_terminal_size(app);
                // Re-probe the display scale: the window may have moved to a
                // monitor with a different DPI, and the RDP viewport request
                // is in physical pixels.
                return window::scale_factor(window).map(Message::DisplayScale);
            }
        }
        Message::DisplayScale(scale) => {
            if (scale - app.display_scale).abs() > f32::EPSILON && scale > 0.1 {
                app.display_scale = scale;
                // The physical-pixel viewport changed even though the logical
                // window did not.
                maybe_resize_active_rdp(app);
            }
        }
        Message::ToggleSidebar => {
            app.sidebar_visible = !app.sidebar_visible;
            sync_terminal_size(app);
        }
        Message::BeginSidebarDrag => app.sidebar_dragging = true,
        Message::SidebarDragMove(x) => {
            if app.sidebar_dragging {
                app.sidebar_width = x.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
                sync_terminal_size(app);
            }
        }
        Message::EndSidebarDrag => app.sidebar_dragging = false,
        Message::SplitPane => {
            split_pane(app);
        }
        Message::ClosePane(index) => {
            close_pane(app, index);
        }
        Message::FocusPane(index) => {
            // Clicking into the workspace saves any in-place rename (click-away).
            commit_inline_rename(app);
            focus_pane(app, index);
        }
        Message::PaneMousePressed(index) => {
            commit_inline_rename(app);
            focus_pane(app, index);
            app.terminal_context_menu = false;
            if mouse_reporting_active(app) {
                app.mouse_button_down = true;
                app.mouse_report_cell = app.terminal_pointer;
                send_mouse_report(app, 0, true, false);
                return Task::none();
            }
            // Begin a selection at the pointer the pane's on_move just recorded
            // (single click = drag-select, double = word, triple = line).
            begin_terminal_click(app);
        }
        Message::PaneRightPressed(index) => {
            focus_pane(app, index);
            app.terminal_selecting = false;
            if app.right_click_paste {
                return clipboard::read().map(Message::ClipboardPasted);
            }
            app.context_menu_pos = app.cursor_pos;
            app.terminal_context_menu = true;
        }
        Message::PanePointerMoved(index, point) => {
            let terminal_point = terminal_point_from_cursor(app, point);
            app.terminal_pointer = Some(terminal_point);
            // Anchor the floating context menu using this pane's screen origin,
            // not the single-pane offset.
            let origin = pane_layout(app).pane_body_origin(index);
            app.cursor_pos = Point::new(origin.x + point.x, origin.y + point.y);

            if maybe_report_mouse_motion(app) {
                return Task::none();
            }
            if app.terminal_selecting {
                extend_terminal_selection(app, point);
            }
        }
        Message::TerminalPointerMoved(point) => {
            let terminal_point = terminal_point_from_cursor(app, point);
            app.terminal_pointer = Some(terminal_point);
            // Track the window-absolute cursor so a right-click can anchor the
            // floating terminal context menu at the pointer.
            let terminal_left = if app.sidebar_visible {
                app.sidebar_width + SIDEBAR_DIVIDER_WIDTH
            } else {
                0.0
            };
            let terminal_top = MENU_BAR_HEIGHT + TOOLBAR_HEIGHT + TAB_BAR_HEIGHT;
            app.cursor_pos = Point::new(point.x + terminal_left, point.y + terminal_top);

            if maybe_report_mouse_motion(app) {
                return Task::none();
            }
            if app.terminal_selecting {
                extend_terminal_selection(app, point);
            }
        }
        Message::CursorBlink => {
            app.cursor_blink_on = !app.cursor_blink_on;
        }
        Message::BeginScrollbarDrag => {
            app.scrollbar_dragging = true;
            app.terminal_context_menu = false;
        }
        Message::ScrollbarDragMove(window_y) => {
            if app.scrollbar_dragging {
                scrollbar_drag_to(app, window_y);
            }
        }
        Message::EndScrollbarDrag => app.scrollbar_dragging = false,
        Message::SelectionCursorMoved(position) => {
            // The pane's own `on_move` stops at its bounds; this arrives from the
            // runtime, so a drag past the edge keeps extending (and arms the edge
            // auto-scroll) instead of freezing.
            if app.terminal_selecting {
                let local = window_to_pane_local(app, position);
                extend_terminal_selection(app, local);
            }
        }
        Message::SelectionAutoScroll => {
            selection_autoscroll_tick(app);
        }
        Message::TerminalScrolled(delta) => {
            app.terminal_focused = true;
            // Ctrl+wheel zooms the terminal font (wheel up = larger), like most
            // terminal emulators — this takes priority over scrolling/reporting.
            if app.modifiers.control() {
                if let Some(lines) = scroll_delta_to_rows(delta) {
                    step_font_size(app, if lines > 0 { 1 } else { -1 });
                    app.notice = format!("终端字号 {}px", app.font_size as i32);
                }
                return Task::none();
            }
            // Forward the wheel to a mouse-reporting app instead of scrolling
            // local history.
            if mouse_reporting_active(app) {
                if let Some(lines) = scroll_delta_to_rows(delta) {
                    let button = if lines > 0 { 64 } else { 65 };
                    for _ in 0..lines.unsigned_abs().min(5) {
                        send_mouse_report(app, button, true, false);
                    }
                }
                return Task::none();
            }
            if let Some(lines) = scroll_delta_to_rows(delta) {
                apply_terminal_scroll(app, TerminalScrollAction::Lines(lines));
            }
        }
        Message::BeginTerminalSelection => {
            // Clicking into the terminal saves any in-place rename (click-away).
            commit_inline_rename(app);
            app.terminal_focused = true;
            app.terminal_context_menu = false;
            // Mouse-reporting apps (vim/tmux/htop) want the click, not a local
            // selection.
            if mouse_reporting_active(app) {
                app.mouse_button_down = true;
                app.mouse_report_cell = app.terminal_pointer;
                send_mouse_report(app, 0, true, false);
                return Task::none();
            }
            begin_terminal_click(app);
        }
        Message::EndTerminalSelection => {
            // A release of a mouse-reporting click sends the button-up report.
            if app.mouse_button_down && mouse_reporting_active(app) {
                app.mouse_button_down = false;
                send_mouse_report(app, 0, false, false);
                return Task::none();
            }
            app.mouse_button_down = false;
            app.terminal_selecting = false;
            app.selection_autoscroll = 0;
            if app
                .terminal_selection
                .is_some_and(|selection| selection.start == selection.end)
            {
                app.terminal_selection = None;
            }
            // Copy-on-select (PuTTY-style): a completed, non-empty selection goes
            // straight to the clipboard.
            if app.copy_on_select && app.terminal_selection.is_some() {
                let text = selected_terminal_text(app);
                if !text.is_empty() {
                    app.notice = String::from("已复制选区到剪贴板");
                    return clipboard::write(text);
                }
            }
        }
        Message::ShowTerminalContextMenu => {
            app.terminal_focused = true;
            app.terminal_selecting = false;
            // Right-click-paste (PuTTY-style): skip the menu and paste directly.
            if app.right_click_paste {
                return clipboard::read().map(Message::ClipboardPasted);
            }
            app.context_menu_pos = app.cursor_pos;
            app.terminal_context_menu = true;
        }
        Message::HideTerminalContextMenu => {
            app.terminal_context_menu = false;
        }
        Message::CopyTerminalSelection => {
            let text = selected_terminal_text(app);
            app.terminal_context_menu = false;
            if !text.is_empty() {
                app.notice = String::from("已复制终端选区");
                return clipboard::write(text);
            }
            app.notice = String::from("没有可复制的终端选区");
        }
        Message::PasteIntoTerminal => {
            app.terminal_context_menu = false;
            return clipboard::read().map(Message::ClipboardPasted);
        }
        Message::ClipboardPasted(contents) => {
            if let Some(contents) = contents {
                if contents.is_empty() {
                    return Task::none();
                }
                let multiline = contents.contains('\n') || contents.contains('\r');
                let bracketed = app.manager.active_bracketed_paste();
                // Bracketed paste already stops the shell from auto-running the
                // pasted block, so only the un-bracketed multi-line case needs a
                // guard.
                if app.confirm_multiline_paste && multiline && !bracketed {
                    app.pending_paste = Some(contents);
                    app.paste_confirm_open = true;
                } else {
                    perform_paste(app, &contents, bracketed);
                }
            }
        }
        Message::ConfirmPaste => {
            app.paste_confirm_open = false;
            if let Some(contents) = app.pending_paste.take() {
                let bracketed = app.manager.active_bracketed_paste();
                perform_paste(app, &contents, bracketed);
            }
        }
        Message::CancelPaste => {
            app.paste_confirm_open = false;
            app.pending_paste = None;
            app.notice = String::from("已取消粘贴");
        }
        Message::ToggleConfirmMultilinePaste(enabled) => {
            app.confirm_multiline_paste = enabled;
        }
        Message::TerminalJumpToBottom => {
            apply_terminal_scroll(app, TerminalScrollAction::Bottom);
        }
        Message::OpenSelectedProfile => {
            open_selected_mock_tab(app);
        }
        Message::ConnectSelectedProfile => {
            connect_or_prompt(app);
        }
        Message::RetryActiveSession => {
            retry_active_session(app);
        }
        Message::TabPressed(session_id) => {
            // Clicking a tab activates it and arms a possible drag-reorder.
            // It also leaves the host list, which shares this tab bar — without
            // that the grid would stay on screen with a session selected behind
            // it, and the two would disagree about what is in front.
            app.main_view = MainView::Terminal;
            activate_session(app, session_id);
            app.dragged_tab = Some(session_id);
        }
        Message::TabDragOver(session_id) => {
            // Live reorder: as the held tab is dragged over a neighbour, move it
            // there immediately so it visibly slides under the cursor (the
            // dragged tab stays active/highlighted, so the motion is obvious).
            if let Some(dragged) = app.dragged_tab {
                if dragged != session_id {
                    app.manager.move_session(dragged, session_id);
                }
            }
        }
        Message::TabReleased => {
            app.dragged_tab = None;
        }
        Message::CloseSession(session_id) => {
            app.tab_context_menu = None;
            app.manager.close(session_id);
            app.terminal_scroll_offset = 0;
            app.terminal_selection = None;
            app.terminal_context_menu = false;
            app.notice = String::from("标签已关闭");
        }
        Message::RenameSessionPrompt(session_id) => {
            app.tab_context_menu = None;
            let current = app
                .manager
                .session_summary(session_id)
                .map(|summary| summary.title)
                .unwrap_or_default();
            app.session_rename_draft = current;
            app.renaming_session = Some(session_id);
            app.terminal_focused = false;
        }
        Message::ShowTabContextMenu(session_id) => {
            // Anchor the floating menu at the cursor (last tracked position).
            app.context_menu_pos = app.cursor_pos;
            app.tab_context_menu = Some(session_id);
            app.profile_context_menu = None;
            app.group_context_menu = None;
            app.terminal_context_menu = false;
        }
        Message::HideTabContextMenu => {
            app.tab_context_menu = None;
        }
        Message::DisconnectSession(session_id) => {
            app.tab_context_menu = None;
            if let Err(error) = app.manager.disconnect(session_id) {
                app.last_error = Some(error.to_string());
            } else {
                app.notice = String::from("已断开连接");
            }
        }
        Message::ReconnectSession(session_id) => {
            app.tab_context_menu = None;
            if let Err(error) = app.manager.reconnect(session_id) {
                app.last_error = Some(error.to_string());
            } else {
                sync_terminal_size(app);
                app.notice = String::from("正在重新连接…");
            }
        }
        Message::CloneSessionFromTab(session_id) => {
            app.tab_context_menu = None;
            // RDP keeps no password in session state (vault-only), so clone it like
            // a fresh connect: load the vault password and open a new RDP session,
            // else prompt for it. `clone_session` refuses RDP for this reason.
            if app.manager.session_is_rdp(session_id) {
                let profile_id = app
                    .manager
                    .sessions()
                    .into_iter()
                    .find(|summary| summary.id == session_id)
                    .map(|summary| summary.profile_id);
                if let Some(profile_id) = profile_id {
                    let stored = app
                        .credential_store
                        .load_profile_password(profile_id)
                        .ok()
                        .flatten();
                    match stored {
                        Some(password) => {
                            let (rw, rh) = rdp_viewport_size(app);
                            match app.manager.open_live_rdp_session(profile_id, password, rw, rh) {
                                Ok(_) => {
                                    remember_recent_host(app, profile_id);
                                    app.rdp_target_size = (rw > 0).then_some((rw, rh));
                                    app.terminal_focused = true;
                                    app.rdp_frame_generation = 0;
                                    app.last_error = None;
                                    app.notice = String::from("已克隆 RDP 会话");
                                }
                                Err(error) => app.last_error = Some(error.to_string()),
                            }
                        }
                        None => {
                            select_profile(app, profile_id);
                            open_connection_dialog(app);
                        }
                    }
                }
                return Task::none();
            }
            match app.manager.clone_session(session_id) {
                Ok(_) => {
                    app.terminal_focused = true;
                    app.terminal_scroll_offset = 0;
                    app.terminal_selection = None;
                    app.terminal_context_menu = false;
                    sync_terminal_size(app);
                    app.notice = String::from("已克隆会话");
                }
                Err(error) => app.last_error = Some(error.to_string()),
            }
        }
        Message::SessionRenameChanged(value) => {
            app.session_rename_draft = value;
        }
        Message::ConfirmRenameSession => {
            if let Some(session_id) = app.renaming_session.take() {
                app.manager
                    .rename_session(session_id, app.session_rename_draft.clone());
            }
        }
        Message::CancelRenameSession => {
            app.renaming_session = None;
        }
        Message::DisconnectActive => {
            disconnect_active(app);
        }
        Message::SendTerminalInput => {
            return send_terminal_input(app);
        }
        Message::ToggleCommandWindow => {
            app.command_window_open = !app.command_window_open;
            if app.command_window_open {
                app.command_history_pos = None;
                return focus_command_input();
            }
        }
        Message::CommandTargetToggled => {
            app.command_target = app.command_target.toggled();
            app.notice = format!("命令窗口目标：{}", app.command_target.label());
        }
        Message::ToggleCommandSendImmediately => {
            app.command_send_immediately = !app.command_send_immediately;
            app.notice = if app.command_send_immediately {
                String::from("命令窗口：逐字符即时发送")
            } else {
                String::from("命令窗口：回车整行发送")
            };
        }
        Message::CommandHistoryPrev => {
            command_history_step(app, -1);
            return focus_command_input();
        }
        Message::CommandHistoryNext => {
            command_history_step(app, 1);
            return focus_command_input();
        }
        Message::ClearActiveTerminal => {
            clear_active_terminal(app);
        }
        Message::ClearError => {
            app.last_error = None;
        }
        Message::CloseSnippets => {
            app.snippets_open = false;
        }
        Message::SnippetNameChanged(value) => {
            app.terminal_focused = false;
            app.snippet_name_draft = value;
        }
        Message::SnippetCommandChanged(value) => {
            app.terminal_focused = false;
            app.snippet_command_draft = value;
        }
        Message::AddSnippet => {
            let name = app.snippet_name_draft.trim().to_string();
            let command = app.snippet_command_draft.trim().to_string();
            if !command.is_empty() {
                app.snippets.push(Snippet {
                    name: if name.is_empty() { command.clone() } else { name },
                    command,
                });
                app.snippet_name_draft.clear();
                app.snippet_command_draft.clear();
            }
        }
        Message::DeleteSnippet(index) => {
            if index < app.snippets.len() {
                app.snippets.remove(index);
            }
        }
        Message::SendSnippet(index) => {
            if let Some(snippet) = app.snippets.get(index) {
                let name = snippet.name.clone();
                let mut bytes = snippet.command.clone().into_bytes();
                bytes.push(b'\r');
                send_terminal_bytes(app, bytes);
                app.notice = format!("已发送片段: {name}");
            }
        }
        Message::CloseSearch => {
            app.search_open = false;
            app.search_matches.clear();
            app.search_index = None;
            app.terminal_focused = true;
        }
        Message::SearchQueryChanged(query) => {
            app.search_query = query;
            recompute_search(app);
        }
        Message::SearchNext => {
            step_search(app, 1);
        }
        Message::SearchPrev => {
            step_search(app, -1);
        }
        Message::CheckForUpdates => {
            return begin_update_check(app);
        }
        Message::UpdateChecked(result) => {
            app.update_state = match result {
                Ok(Some(info)) => UpdateState::Available(info),
                Ok(None) => UpdateState::UpToDate,
                Err(error) => UpdateState::Error(error),
            };
        }
        Message::AutoUpdateChecked(result) => {
            // Silent on startup: only surface the dialog when a newer version
            // actually exists.
            if let Ok(Some(info)) = result {
                app.update_state = UpdateState::Available(info);
                app.update_dialog_open = true;
            }
        }
        Message::KeyringMigrated(imported) => {
            // Mark it done so it never runs again; `persist_settings_if_changed`
            // writes the flag on the next Tick.
            app.keyring_migrated = true;
            if imported > 0 {
                app.notice =
                    format!("已从系统密钥环导入 {imported} 条密码到配置目录(可随 Dropbox 同步)");
                // A secret for the selected profile may have just arrived; refresh
                // so the connect form reflects it.
                load_selected_profile(app);
            }
        }
        Message::ToggleAutoCheckUpdates(enabled) => {
            app.auto_check_updates = enabled;
        }
        Message::ToggleAutoAcceptHostKeys(enabled) => {
            app.auto_accept_host_keys = enabled;
            app.manager.set_auto_accept_host_keys(enabled);
            app.notice = if enabled {
                String::from("已开启：自动信任新主机密钥")
            } else {
                String::from("已关闭：新主机密钥将逐个确认")
            };
        }
        Message::ToggleRdpClipboard(enabled) => {
            app.rdp_clipboard = enabled;
            app.manager.set_rdp_clipboard(enabled);
            // Turning it off must also drop what the poll already captured,
            // otherwise the last thing copied stays queued for the next remote
            // that asks — the setting would read as off while still leaking.
            if !enabled {
                app.rdp_clipboard_offered = None;
            }
            app.notice = if enabled {
                String::from("已开启：RDP 共享剪贴板（下次连接生效）")
            } else {
                String::from("已关闭：RDP 不再共享剪贴板（下次连接生效）")
            };
        }
        Message::StartUpdateDownload => {
            if let UpdateState::Available(info) = &app.update_state {
                let url = info.installer_url.clone();
                let name = info.installer_name.clone();
                app.update_state = UpdateState::Downloading;
                return Task::perform(
                    download_installer(url, name),
                    Message::UpdateDownloaded,
                );
            }
        }
        Message::UpdateDownloaded(result) => match result {
            Ok(path) => match launch_silent_update(&path).spawn() {
                Ok(_) => {
                    app.update_state = UpdateState::Launched;
                    app.notice = String::from(
                        "正在后台静默安装更新，完成后 Adit 会自动重启（可能需要确认一次 UAC）",
                    );
                }
                Err(error) => {
                    app.update_state = UpdateState::Error(format!("无法启动安装程序: {error}"));
                }
            },
            Err(error) => {
                app.update_state = UpdateState::Error(error);
            }
        },
        Message::CloseUpdateDialog => {
            app.update_dialog_open = false;
        }
        Message::OpenReleaseNotes(url) => {
            open_url(app, &url);
        }
    }

    Task::none()
}

/// Cut a decoded desktop frame into tiles small enough that iced_wgpu uploads
/// each one synchronously.
///
/// One 8.8 MB image is over the renderer's 2 MiB synchronous threshold, so it
/// goes to an async worker and is not drawable on the frame it arrives — the
/// renderer simply skips it (`image/mod.rs`: `if let Some(..) =
/// cache.upload_raster(..)`, with no else) and the black container shows
/// through. Tiles of 512x512x4 = 1 MiB each stay on the synchronous path, so
/// every frame is complete the moment it lands: no gap to paper over, and
/// therefore no stale underlay to ghost during scrolling or leave remnants
/// when a window is minimised.
pub(crate) fn split_into_tiles(frame: &adit_session::RdpFrame) -> Vec<RdpTile> {
    let (fw, fh) = (usize::from(frame.width), usize::from(frame.height));
    if fw == 0 || fh == 0 || frame.rgba.len() < fw * fh * 4 {
        return Vec::new();
    }
    let step = usize::from(RDP_TILE);
    let mut tiles = Vec::with_capacity(fw.div_ceil(step) * fh.div_ceil(step));
    for ty in (0..fh).step_by(step) {
        let h = step.min(fh - ty);
        for tx in (0..fw).step_by(step) {
            let w = step.min(fw - tx);
            let mut rgba = Vec::with_capacity(w * h * 4);
            for row in 0..h {
                let start = ((ty + row) * fw + tx) * 4;
                rgba.extend_from_slice(&frame.rgba[start..start + w * 4]);
            }
            tiles.push(RdpTile {
                y: u16::try_from(ty).unwrap_or(u16::MAX),
                width: u16::try_from(w).unwrap_or(u16::MAX),
                height: u16::try_from(h).unwrap_or(u16::MAX),
                handle: iced::widget::image::Handle::from_rgba(
                    u32::try_from(w).unwrap_or(0),
                    u32::try_from(h).unwrap_or(0),
                    rgba,
                ),
            });
        }
    }
    tiles
}

/// Write the app-side desktop framebuffer to disk, under `ADIT_RDP_FRAMES`.
///
/// This is the one instrument that separates "the helper composited it wrong"
/// from "the renderer displayed it wrong": the bytes here are exactly what the
/// helper produced and exactly what the tiles are cut from. A ghost visible on
/// screen but ABSENT from these PNGs is a presentation bug; a ghost present in
/// them came over the wire.
///
/// Best-effort and opt-in — a desktop framebuffer is a picture of the user's
/// screen, and at 8.8 MB a frame this is not something to do by default.
fn dump_rdp_frame(frame: &adit_session::RdpFrame) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNT: AtomicU32 = AtomicU32::new(0);

    let Some(dir) = std::env::var_os("ADIT_RDP_FRAMES") else {
        return;
    };
    // Ring, not a prefix: the artefact worth capturing is whatever the user
    // was looking at when they stopped, and a first-40-frames cap only ever
    // catches the logon screen.
    const RING: u32 = 40;
    let index = COUNT.fetch_add(1, Ordering::Relaxed) % RING;
    let path = std::path::PathBuf::from(dir).join(format!("frame-{index:03}.png"));
    // PNG by hand would need an encoder here; a raw dump plus its dimensions is
    // enough for the offline comparison and keeps this dependency-free.
    let meta = format!("{}x{}
", frame.width, frame.height);
    let _ = std::fs::write(path.with_extension("txt"), meta);
    let _ = std::fs::write(path.with_extension("raw"), &frame.rgba);
}
