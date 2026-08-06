use super::*;

pub(crate) fn open_selected_mock_tab(app: &mut AditApp) {
    if let Some(profile_id) = save_profile_from_form(app, false) {
        match app.manager.open_mock_session(profile_id) {
            Ok(_) => {
                app.terminal_focused = true;
                app.terminal_scroll_offset = 0;
                app.terminal_selection = None;
                app.terminal_context_menu = false;
                sync_terminal_size(app);
                app.last_error = None;
                app.notice = String::from(t("已打开演示标签"));
            }
            Err(error) => app.last_error = Some(error.to_string()),
        }
    }
}

/// Connect to a profile directly (used by double-click). Uses a stored password
/// when present; for key/agent/auto auth it connects with no password; only
/// password auth without a stored secret falls back to the connection dialog.
/// How many hosts the "recent" band keeps. Enough to fill a row of cards
/// without the band turning into a second copy of the whole list.
pub(crate) const RECENT_HOSTS_MAX: usize = 6;

/// Move `profile_id` to the front of the recent list.
///
/// Called wherever a session actually opens, which is more places than it looks.
/// `connect_profile` is only the direct route: SSH and RDP fall through to the
/// password dialog and the session opens from *its* handler, which
/// `connect_profile` never returns to. Recording in one place read as obviously
/// sufficient and left the band permanently empty for SSH — the protocol nearly
/// every host uses.
pub(crate) fn remember_recent_host(app: &mut AditApp, profile_id: ProfileId) {
    app.recent_hosts.retain(|id| *id != profile_id);
    app.recent_hosts.insert(0, profile_id);
    app.recent_hosts.truncate(RECENT_HOSTS_MAX);
}

pub(crate) fn connect_profile(app: &mut AditApp) {
    let Some(profile_id) = save_profile_from_form(app, false) else {
        return;
    };
    remember_recent_host(app, profile_id);
    let Some(profile) = app.manager.profile(profile_id).cloned() else {
        app.last_error = Some(String::from(t("请选择要连接的会话配置")));
        return;
    };

    // RDP opens a native graphical session in a tab (via the helper process).
    // NLA needs a password: use the stored one, else prompt through the dialog.
    if profile.protocol == Protocol::Rdp {
        let stored = app
            .credential_store
            .load_profile_password(profile_id)
            .ok()
            .flatten();
        let Some(password) = stored else {
            open_connection_dialog(app);
            return;
        };
        let (rw, rh) = rdp_viewport_size(app);
        match app.manager.open_live_rdp_session(profile_id, password, rw, rh) {
            Ok(_) => {
                app.rdp_target_size = (rw > 0).then_some((rw, rh));
                app.connection_dialog = None;
                app.password.clear();
                app.remember_connection_password = false;
                app.terminal_focused = true;
                app.rdp_frame_generation = 0;
                app.last_error = None;
                app.notice = format!("RDP 会话已开始连接: {}", profile.endpoint());
            }
            Err(error) => app.last_error = Some(error.to_string()),
        }
        return;
    }

    // Non-SSH terminal protocols (local shell, serial) need no credential.
    let password = if profile.protocol == Protocol::Ssh {
        let stored = app
            .credential_store
            .load_profile_password(profile_id)
            .ok()
            .flatten();
        match stored {
            Some(password) => password,
            None => {
                if profile.auth_method == AuthMethod::Password {
                    open_connection_dialog(app);
                    return;
                }
                String::new()
            }
        }
    } else {
        String::new()
    };

    // The key passphrase is saved separately in the vault (via the profile
    // editor), so it's loaded here regardless of how the password was obtained.
    let passphrase = app
        .credential_store
        .load_profile_passphrase(profile_id)
        .ok()
        .flatten()
        .unwrap_or_default();

    match app
        .manager
        .open_live_ssh_session(profile_id, password, passphrase)
    {
        Ok(_) => {
            app.connection_dialog = None;
            app.password.clear();
            app.remember_connection_password = false;
            app.terminal_focused = true;
            app.terminal_scroll_offset = 0;
            app.terminal_selection = None;
            app.terminal_context_menu = false;
            sync_terminal_size(app);
            if profile.protocol == Protocol::Ssh {
                app.manager.start_profile_tunnels(profile_id);
            }
            app.last_error = None;
            app.notice = if profile.protocol == Protocol::Ssh {
                format!("SSH 会话已开始连接: {}", profile.endpoint())
            } else {
                format!("已启动{}", profile.protocol.label())
            };
        }
        Err(error) => {
            app.last_error = Some(error.to_string());
        }
    }
}

/// A connection was rejected on credentials: re-open the password dialog for that
/// profile so the user can correct them, instead of leaving them with just an
/// error in the transcript. Unlike [`open_connection_dialog`] this targets the
/// failed session's profile rather than the sidebar form, and starts with an empty
/// field — whatever was there is exactly what the server just refused.
pub(crate) fn open_password_reprompt(app: &mut AditApp, profile_id: ProfileId, reason: &str) {
    let Some(profile) = app.manager.profile(profile_id).cloned() else {
        return;
    };
    // Only the credential-based protocols have a password to re-ask for.
    if !matches!(profile.protocol, Protocol::Ssh | Protocol::Rdp) {
        return;
    }

    let endpoint = profile.endpoint();
    app.connection_dialog = Some(ConnectionDialog {
        profile_id,
        title: profile.name,
        endpoint,
        auth_method: profile.auth_method,
        identity_file: profile.identity_file,
    });
    app.password.clear();
    // Save by default (SecureCRT-style), same as the normal connect dialog — so a
    // corrected password is remembered without the user re-ticking the box.
    app.remember_connection_password = true;
    app.last_error = Some(tf("认证失败，请重新输入密码: {}", &[&reason]));
}

/// Drain a credential rejection and re-open the password dialog for it. Skipped
/// while a dialog is already open (the flag stays set for the next poll) so we
/// never wipe what the user is currently typing.
pub(crate) fn sync_auth_retry(app: &mut AditApp) {
    if app.connection_dialog.is_some() {
        return;
    }
    if let Some((_session_id, profile_id, reason)) = app.manager.take_auth_rejection() {
        open_password_reprompt(app, profile_id, &reason);
    }
}

pub(crate) fn open_connection_dialog(app: &mut AditApp) {
    let Some(profile_id) = save_profile_from_form(app, false) else {
        return;
    };
    let Some(profile) = app.manager.profile(profile_id).cloned() else {
        app.last_error = Some(String::from(t("请选择要连接的会话配置")));
        return;
    };

    // SSH and RDP both need the password dialog (RDP uses NLA). Only the
    // credential-free protocols (local shell, serial) connect directly here.
    //
    // RDP used to fall into this `connect_profile` branch: when no password was
    // stored, `connect_profile` called back into `open_connection_dialog`, which
    // called `connect_profile` again — unbounded mutual recursion that froze the
    // UI ("Not Responding"), or overflowed the stack outright on smaller stacks.
    if !matches!(profile.protocol, Protocol::Ssh | Protocol::Rdp) {
        connect_profile(app);
        return;
    }

    let endpoint = profile.endpoint();
    app.connection_dialog = Some(ConnectionDialog {
        profile_id,
        title: profile.name,
        endpoint,
        auth_method: profile.auth_method,
        identity_file: profile.identity_file,
    });

    match app.credential_store.load_profile_password(profile_id) {
        Ok(Some(password)) => {
            app.password = password;
            app.remember_connection_password = true;
            app.last_error = None;
            app.notice = String::from(t("已载入已保存的密码"));
        }
        Ok(None) => {
            app.password.clear();
            // Auto-save whatever the user types, no opt-in needed (SecureCRT-style).
            app.remember_connection_password = true;
            app.last_error = None;
            app.notice = String::from(t("请输入本次连接的密码或 passphrase"));
        }
        Err(error) => {
            app.password.clear();
            app.remember_connection_password = true;
            app.last_error = Some(tf("读取已保存的密码失败: {}", &[&error]));
            app.notice = String::from(t("请输入本次连接的密码或 passphrase"));
        }
    }

    app.terminal_focused = false;
    app.terminal_context_menu = false;
    app.profile_context_menu = None;
    app.group_context_menu = None;
}

/// Connect straight away when the profile's password is already stored,
/// otherwise raise the dialog.
///
/// `connect_profile` has always been able to start an RDP or SSH session from
/// a stored password without asking; it was simply never reachable, because
/// `open_connection_dialog` returns early for both protocols. That early
/// return exists to break a mutual recursion (`connect_profile` with no
/// password called back into the dialog, which called `connect_profile`
/// again, freezing the UI), and it is why every connect asked for a password
/// that was already on disk.
///
/// Calling `connect_profile` *only* when a password exists cannot re-enter
/// that cycle: the branch that calls back into the dialog is the no-password
/// one. Retry deliberately keeps prompting (see `retry_active_session`) --
/// otherwise a stored password that has gone stale could never be corrected.
pub(crate) fn connect_or_prompt(app: &mut AditApp) {
    let Some(profile_id) = app.selected_profile else {
        open_connection_dialog(app);
        return;
    };
    let Some(profile) = app.manager.profile(profile_id) else {
        open_connection_dialog(app);
        return;
    };
    let stored = app
        .credential_store
        .load_profile_password(profile_id)
        .ok()
        .flatten()
        .is_some_and(|password| !password.is_empty());
    let can_auto = match profile.protocol {
        Protocol::Rdp => stored,
        // A key-based SSH profile needs no password at all; a password one
        // needs the stored secret.
        Protocol::Ssh => stored || profile.auth_method != AuthMethod::Password,
        _ => true,
    };
    if can_auto {
        // Switching to the session view is part of connecting, not a side
        // effect of it: the RDP key path is gated on `main_view ==
        // Terminal` while the pointer path is not, so connecting from the
        // host grid without this produced a desktop that could be clicked
        // but not typed into — the exact symptom that gate was added to fix.
        app.main_view = MainView::Terminal;
        connect_profile(app);
    } else {
        open_connection_dialog(app);
    }
}

pub(crate) fn confirm_connection(app: &mut AditApp) {
    let Some(dialog) = app.connection_dialog.clone() else {
        open_connection_dialog(app);
        return;
    };

    // RDP opens a native graphical tab (helper process) instead of an SSH shell.
    let is_rdp =
        app.manager.profile(dialog.profile_id).map(|p| p.protocol) == Some(Protocol::Rdp);
    if is_rdp {
        let credential_warning = sync_connection_password(app, dialog.profile_id).err();
        let (rw, rh) = rdp_viewport_size(app);
        match app
            .manager
            .open_live_rdp_session(dialog.profile_id, app.password.clone(), rw, rh)
        {
            Ok(_) => {
                remember_recent_host(app, dialog.profile_id);
                app.rdp_target_size = (rw > 0).then_some((rw, rh));
                app.connection_dialog = None;
                app.password.clear();
                app.remember_connection_password = false;
                app.terminal_focused = true;
                app.rdp_frame_generation = 0;
                app.last_error = credential_warning
                    .as_ref()
                    .map(|error| tf("保存密码失败: {}", &[&error]));
                app.notice = format!("RDP 会话已开始连接: {}", dialog.endpoint);
            }
            Err(error) => app.last_error = Some(error.to_string()),
        }
        return;
    }

    let credential_warning = sync_connection_password(app, dialog.profile_id).err();
    let passphrase = app
        .credential_store
        .load_profile_passphrase(dialog.profile_id)
        .ok()
        .flatten()
        .unwrap_or_default();

    match app
        .manager
        .open_live_ssh_session(dialog.profile_id, app.password.clone(), passphrase)
    {
        Ok(_) => {
            remember_recent_host(app, dialog.profile_id);
            app.connection_dialog = None;
            app.password.clear();
            app.remember_connection_password = false;
            app.terminal_focused = true;
            app.terminal_scroll_offset = 0;
            app.terminal_selection = None;
            app.terminal_context_menu = false;
            sync_terminal_size(app);
            app.manager.start_profile_tunnels(dialog.profile_id);
            app.last_error = credential_warning
                .as_ref()
                .map(|error| tf("保存密码失败: {}", &[&error]));
            app.notice = if credential_warning.is_some() {
                format!("SSH 会话已开始连接: {}；系统凭据未保存", dialog.endpoint)
            } else {
                format!("SSH 会话已开始连接: {}", dialog.endpoint)
            };
        }
        Err(error) => {
            app.last_error = Some(error.to_string());
        }
    }
}

pub(crate) fn retry_active_session(app: &mut AditApp) {
    let Some(summary) = app.manager.active_session_summary() else {
        app.last_error = Some(String::from(t("没有可重连的活动标签")));
        return;
    };

    if !matches!(
        summary.status,
        SessionStatus::Error | SessionStatus::Disconnected
    ) {
        app.notice = String::from(t("当前会话仍在连接或已连接，无需重连"));
        return;
    }

    select_profile(app, summary.profile_id);
    app.manager.close(summary.id);
    app.terminal_scroll_offset = 0;
    app.terminal_selection = None;
    app.terminal_context_menu = false;
    app.notice = format!("准备重连: {}", summary.endpoint);
    // Reconnect the way the host list connects: straight through when the
    // profile already has what it needs. Opening the dialog unconditionally
    // asked for a password that was sitting in the credential store, which made
    // 重连 slower than double-clicking the host it was offering to reconnect.
    connect_or_prompt(app);
}

pub(crate) fn sync_connection_password(
    app: &mut AditApp,
    profile_id: ProfileId,
) -> Result<(), adit_storage::CredentialError> {
    if app.remember_connection_password && !app.password.is_empty() {
        app.credential_store
            .save_profile_password(profile_id, &app.password)
    } else {
        app.credential_store.delete_profile_password(profile_id)
    }
}

pub(crate) fn disconnect_active(app: &mut AditApp) {
    if let Some(session_id) = app.manager.active_session() {
        match app.manager.disconnect(session_id) {
            Ok(()) => {
                app.last_error = None;
                app.notice = String::from(t("已请求断开当前会话"));
            }
            Err(error) => app.last_error = Some(error.to_string()),
        }
    } else {
        app.last_error = Some(String::from(t("没有活动会话")));
    }
}

/// Send the command-window line to its target (active session or broadcast),
/// append a carriage return, remember it in history, and keep focus in the box.
pub(crate) fn send_terminal_input(app: &mut AditApp) -> Task<Message> {
    let line = app.terminal_input.clone();
    // In send-immediately mode the characters were already sent as typed, so
    // Enter only needs to send the newline that submits the command.
    let payload = if app.command_send_immediately {
        String::from("\r")
    } else {
        if line.trim().is_empty() {
            return Task::none();
        }
        format!("{line}\r")
    };

    let result = match app.command_target {
        CommandTarget::AllSessions => app
            .manager
            .send_input_bytes_broadcast(payload.into_bytes())
            .map(|_| ()),
        CommandTarget::ActiveSession => app.manager.send_input_to_active(payload),
    };

    match result {
        Ok(()) => {
            if !line.trim().is_empty() && app.command_history.last().map(String::as_str) != Some(line.as_str()) {
                app.command_history.push(line);
            }
            app.command_history_pos = None;
            app.terminal_input.clear();
            app.terminal_scroll_offset = 0;
            app.terminal_selection = None;
            app.last_error = None;
            // Keep typing without re-clicking the box.
            if app.command_window_open {
                return focus_command_input();
            }
        }
        Err(error) => {
            app.last_error = Some(error.to_string());
        }
    }
    Task::none()
}

/// Bytes to forward for the change from `old` to `new` in send-immediately mode:
/// the appended suffix when text was typed, DELs when text was erased. Returns
/// `None` for a mid-string edit we can't represent as a simple keystroke.
pub(crate) fn command_input_delta(old: &str, new: &str) -> Option<Vec<u8>> {
    if new.len() > old.len() && new.starts_with(old) {
        Some(new.as_bytes()[old.len()..].to_vec())
    } else if new.len() < old.len() && old.starts_with(new) {
        // One backspace (DEL, 0x7f) per removed character.
        Some(vec![0x7f; old[new.len()..].chars().count()])
    } else if new == old {
        Some(Vec::new())
    } else {
        None
    }
}

/// Send raw bytes to the command window's target(s) without disturbing history.
pub(crate) fn send_command_bytes(app: &mut AditApp, bytes: Vec<u8>) {
    if bytes.is_empty() {
        return;
    }
    app.terminal_scroll_offset = 0;
    let result = match app.command_target {
        CommandTarget::AllSessions => app.manager.send_input_bytes_broadcast(bytes).map(|_| ()),
        CommandTarget::ActiveSession => app.manager.send_input_bytes_to_active(bytes),
    };
    if let Err(error) = result {
        app.last_error = Some(error.to_string());
    }
}

/// Replace the command input with a history entry, stepping `delta` (-1 = older,
/// +1 = newer). Stepping past the newest entry restores an empty line.
pub(crate) fn command_history_step(app: &mut AditApp, delta: i32) {
    if app.command_history.is_empty() {
        return;
    }
    let len = app.command_history.len();
    let next = match app.command_history_pos {
        None if delta < 0 => Some(len - 1),
        None => return,
        Some(pos) => {
            let pos = pos as i32 + delta;
            if pos < 0 {
                Some(0)
            } else if pos as usize >= len {
                None
            } else {
                Some(pos as usize)
            }
        }
    };
    app.command_history_pos = next;
    app.terminal_input = next.map(|i| app.command_history[i].clone()).unwrap_or_default();
}

pub(crate) fn command_input_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("command-window-input")
}

pub(crate) fn focus_command_input() -> Task<Message> {
    iced::advanced::widget::operate(iced::advanced::widget::operation::focusable::focus(
        command_input_id(),
    ))
}

pub(crate) fn send_terminal_bytes(app: &mut AditApp, bytes: Vec<u8>) {
    // Keep the cursor solid while typing — a cursor that blinks out from under the
    // character you just typed reads as a dropped keystroke. It resumes blinking
    // on the next tick once you stop.
    app.cursor_blink_on = true;

    // Broadcast mode fans keystrokes out to every connected session at once.
    if app.broadcast_input {
        app.terminal_scroll_offset = 0;
        app.terminal_selection = None;
        if let Err(error) = app.manager.send_input_bytes_broadcast(bytes) {
            app.last_error = Some(error.to_string());
        }
        return;
    }

    if app.manager.active_session().is_none() {
        return;
    }

    app.terminal_scroll_offset = 0;
    app.terminal_selection = None;

    if let Err(error) = app.manager.send_input_bytes_to_active(bytes) {
        app.last_error = Some(error.to_string());
    }
}

/// Whether the active session has enabled mouse reporting (so mouse events go
/// to the remote instead of doing local selection).
pub(crate) fn mouse_reporting_active(app: &AditApp) -> bool {
    app.manager.active_mouse_mode() != MouseMode::Off
}

/// Encode + send a mouse report to the active session at the current pointer
/// cell (raw send — not broadcast, does not touch the selection).
pub(crate) fn send_mouse_report(app: &mut AditApp, button: u8, press: bool, motion: bool) {
    let Some(point) = app.terminal_pointer else {
        return;
    };
    let sgr = app.manager.active_mouse_sgr();
    let bytes = encode_mouse_event(sgr, button, point.col, point.row, press, motion);
    let _ = app.manager.send_input_bytes_to_active(bytes);
}

/// On pointer motion over a mouse-reporting terminal, send a drag (button held)
/// or any-motion report when the cell changes. Returns true when reporting is
/// active (so the caller skips local selection).
pub(crate) fn maybe_report_mouse_motion(app: &mut AditApp) -> bool {
    if !mouse_reporting_active(app) {
        return false;
    }
    let mode = app.manager.active_mouse_mode();
    if app.terminal_pointer == app.mouse_report_cell {
        return true; // same cell — consumed, nothing new to report
    }
    if app.mouse_button_down && mode.reports_drag() {
        app.mouse_report_cell = app.terminal_pointer;
        send_mouse_report(app, 0, true, true);
    } else if !app.mouse_button_down && mode.reports_any_motion() {
        app.mouse_report_cell = app.terminal_pointer;
        send_mouse_report(app, 3, true, true);
    }
    true
}

/// Whether the active tab's session has dropped (errored or disconnected) and so
/// is a candidate for Enter-to-reconnect.
pub(crate) fn active_session_is_dead(app: &AditApp) -> bool {
    app.manager.active_session_summary().is_some_and(|summary| {
        matches!(
            summary.status,
            SessionStatus::Error | SessionStatus::Disconnected
        )
    })
}

/// Reconnect the active dead session. SSH reconnects in place with its stored
/// credentials (no dialog); RDP / the SFTP shell / a session with nothing stored
/// fall back to the dialog path, which re-asks for the password.
pub(crate) fn reconnect_active_session(app: &mut AditApp) {
    if app.manager.active_can_reconnect() {
        let Some(summary) = app.manager.active_session_summary() else {
            return;
        };
        match app.manager.reconnect(summary.id) {
            Ok(()) => {
                app.terminal_scroll_offset = 0;
                app.terminal_selection = None;
                app.last_error = None;
                app.notice = format!("正在重连: {}", summary.endpoint);
            }
            Err(error) => app.last_error = Some(error.to_string()),
        }
    } else {
        // No in-place reconnect (RDP / SFTP shell): reopen the connection dialog.
        retry_active_session(app);
    }
}

/// A plain Alt+<key> app shortcut (SecureCRT-style, e.g. Alt+P opens SFTP).
/// Matched via the physical key so it works on non-Latin layouts. Alt normally
/// prefixes terminal input with ESC (Meta), so this deliberately claims the combo
/// before the terminal sees it.
/// Send pasted text to the terminal, wrapping it in the bracketed-paste markers
/// (`ESC[200~` … `ESC[201~`) when the app has enabled DEC mode 2004.
pub(crate) fn perform_paste(app: &mut AditApp, contents: &str, bracketed: bool) {
    let mut bytes = normalize_paste(contents);
    if bytes.is_empty() {
        return;
    }
    if bracketed {
        let mut wrapped = Vec::with_capacity(bytes.len() + 12);
        wrapped.extend_from_slice(b"\x1b[200~");
        wrapped.append(&mut bytes);
        wrapped.extend_from_slice(b"\x1b[201~");
        bytes = wrapped;
    }
    send_terminal_bytes(app, bytes);
    app.notice = String::from(t("已粘贴到当前终端"));
}

pub(crate) fn search_input_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("terminal-search")
}

/// A Task that moves keyboard focus to the search input.
pub(crate) fn focus_search_input() -> Task<Message> {
    iced::advanced::widget::operate(iced::advanced::widget::operation::focusable::focus(
        search_input_id(),
    ))
}



pub(crate) fn session_filter_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("sidebar-filter")
}

/// A Task that moves keyboard focus to the sidebar's filter box (Alt+I).
pub(crate) fn focus_session_filter() -> Task<Message> {
    iced::advanced::widget::operate(iced::advanced::widget::operation::focusable::focus(
        session_filter_id(),
    ))
}

/// The shared id of the in-place rename text input (only one folder or session
/// is ever renamed at a time).
pub(crate) fn rename_input_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("sidebar-inline-rename")
}

/// A Task that moves keyboard focus to the in-place rename input.
pub(crate) fn focus_rename_input() -> Task<Message> {
    iced::advanced::widget::operate(iced::advanced::widget::operation::focusable::focus(
        rename_input_id(),
    ))
}

/// Recompute scrollback-search matches over the active session's full buffer
/// (ASCII case-insensitive), then jump to the last (most recent) match.
pub(crate) fn recompute_search(app: &mut AditApp) {
    app.search_matches.clear();
    app.search_index = None;

    let needle: Vec<char> = app.search_query.chars().map(|c| c.to_ascii_lowercase()).collect();
    if needle.is_empty() {
        return;
    }

    let rows_visible = terminal_view_rows(app);
    let total = app.manager.active_snapshot(Viewport::tail(rows_visible)).total_rows;
    if total == 0 {
        return;
    }
    let full = app.manager.active_snapshot(Viewport {
        first_row: 0,
        height: total,
    });

    for (row, line) in full.lines.iter().enumerate() {
        let hay: Vec<char> = line
            .cells
            .iter()
            .flat_map(|cell| cell.text.chars())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        if needle.len() > hay.len() {
            continue;
        }
        let mut i = 0;
        while i + needle.len() <= hay.len() {
            if hay[i..i + needle.len()] == needle[..] {
                app.search_matches.push(SearchMatch {
                    row,
                    col: i,
                    len: needle.len(),
                });
                i += needle.len();
            } else {
                i += 1;
            }
        }
    }

    if !app.search_matches.is_empty() {
        app.search_index = Some(app.search_matches.len() - 1);
        scroll_to_current_match(app);
    }
}

/// Advance the current match by `delta` (wrapping) and scroll it into view.
pub(crate) fn step_search(app: &mut AditApp, delta: i32) {
    let count = app.search_matches.len();
    if count == 0 {
        return;
    }
    let current = app.search_index.unwrap_or(0) as i32;
    let next = (current + delta).rem_euclid(count as i32) as usize;
    app.search_index = Some(next);
    scroll_to_current_match(app);
}

/// Scroll the terminal so the current match sits roughly a third from the top.
pub(crate) fn scroll_to_current_match(app: &mut AditApp) {
    let Some(index) = app.search_index else {
        return;
    };
    let Some(hit) = app.search_matches.get(index).copied() else {
        return;
    };
    let rows_visible = terminal_view_rows(app);
    let total = app.manager.active_snapshot(Viewport::tail(rows_visible)).total_rows;
    let first_visible = hit.row.saturating_sub(rows_visible / 3);
    let offset = total
        .saturating_sub(rows_visible)
        .saturating_sub(first_visible);
    app.terminal_scroll_offset = offset.min(max_terminal_scroll_offset(app));
}

/// Per-visible-line search highlight ranges `(start, end, is_current)` aligned to
/// the snapshot's lines; empty when there is nothing to highlight.
pub(crate) fn search_highlights_for(app: &AditApp, snapshot: &TerminalSnapshot) -> Vec<Vec<(usize, usize, bool)>> {
    if app.search_matches.is_empty() {
        return Vec::new();
    }
    let current = app.search_index.and_then(|i| app.search_matches.get(i)).copied();
    snapshot
        .lines
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let abs = snapshot.first_row + i;
            app.search_matches
                .iter()
                .filter(|m| m.row == abs)
                .map(|m| (m.col, m.col + m.len, Some(*m) == current))
                .collect()
        })
        .collect()
}

pub(crate) fn selected_terminal_text(app: &AditApp) -> String {
    let snapshot = active_terminal_snapshot(app);

    if let Some(selection) = app.terminal_selection {
        let selected = selection_to_text(&snapshot, selection);
        if !selected.is_empty() {
            return selected;
        }
    }

    snapshot_to_text(&snapshot)
}

pub(crate) fn active_terminal_snapshot(app: &AditApp) -> TerminalSnapshot {
    let rows = terminal_view_rows(app);
    let tail = app.manager.active_snapshot(Viewport::tail(rows));

    if app.terminal_scroll_offset == 0 {
        return tail;
    }

    let offset = app
        .terminal_scroll_offset
        .min(max_scroll_offset_for(&tail, rows));
    let first_row = tail.total_rows.saturating_sub(rows).saturating_sub(offset);

    app.manager.active_snapshot(Viewport {
        first_row,
        height: rows,
    })
}

pub(crate) fn terminal_view_rows(app: &AditApp) -> usize {
    usize::from(app.terminal_size.rows).max(1)
}

pub(crate) fn max_terminal_scroll_offset(app: &AditApp) -> usize {
    let rows = terminal_view_rows(app);
    let snapshot = app.manager.active_snapshot(Viewport::tail(rows));
    max_scroll_offset_for(&snapshot, rows)
}

pub(crate) fn clamp_terminal_scroll(app: &mut AditApp) {
    let max_offset = max_terminal_scroll_offset(app);
    app.terminal_scroll_offset = app.terminal_scroll_offset.min(max_offset);
}

pub(crate) fn apply_terminal_scroll(app: &mut AditApp, action: TerminalScrollAction) {
    let max_offset = max_terminal_scroll_offset(app);
    let previous = app.terminal_scroll_offset.min(max_offset);

    let next = match action {
        TerminalScrollAction::Lines(lines) if lines > 0 => {
            previous.saturating_add(lines as usize).min(max_offset)
        }
        TerminalScrollAction::Lines(lines) if lines < 0 => {
            previous.saturating_sub(lines.unsigned_abs() as usize)
        }
        TerminalScrollAction::Lines(_) => previous,
        TerminalScrollAction::Top => max_offset,
        TerminalScrollAction::Bottom => 0,
    };

    app.terminal_scroll_offset = next;
    app.terminal_context_menu = false;

    if next != previous {
        // The selection is anchored in absolute scrollback rows, so scrolling no
        // longer invalidates it — keep it (and let a drag extend through a scroll).
        app.notice = if next == 0 {
            String::from(t("终端已回到底部"))
        } else {
            tf("终端历史: 距底部 {} 行", &[&next])
        };
    }
}

/// Handle a left-press on the terminal grid, deciding — from how quickly it
/// follows the previous press on the same cell — whether it starts a drag
/// selection (single), selects a word (double), or selects a line (triple).
pub(crate) fn begin_terminal_click(app: &mut AditApp) {
    // A fresh press must never inherit the previous drag's auto-scroll, or the
    // view would scroll the instant the button goes down.
    app.selection_autoscroll = 0;
    // `terminal_pointer` is viewport-relative (mouse reporting needs that); the
    // selection is anchored in absolute scrollback rows.
    let viewport_point = app
        .terminal_pointer
        .unwrap_or(TerminalPoint { row: 0, col: 0 });
    let point = TerminalPoint {
        row: viewport_first_row(app).saturating_add(viewport_point.row),
        col: viewport_point.col,
    };
    let now = Instant::now();
    let count = match app.terminal_click {
        Some((last_point, last_time, last_count))
            if last_point == point
                && now.duration_since(last_time) < Duration::from_millis(400) =>
        {
            // 1 -> 2 -> 3 -> back to 1, so a fourth click restarts the cycle.
            (last_count % 3) + 1
        }
        _ => 1,
    };
    app.terminal_click = Some((point, now, count));

    match count {
        2 => {
            select_word_at(app, point);
            // A word/line selection is fixed; don't let the following move extend
            // it character-by-character.
            app.terminal_selecting = false;
        }
        3 => {
            select_line_at(app, point);
            app.terminal_selecting = false;
        }
        _ => {
            app.terminal_selection = Some(TerminalSelection {
                start: point,
                end: point,
            });
            app.terminal_selecting = true;
        }
    }
}

/// Map a window-absolute cursor position into the focused pane's terminal-local
/// space — the coordinates `terminal_point_from_cursor` expects (i.e. relative to
/// that pane's `mouse_area`). `pane_layout` covers the unsplit case too (one pane,
/// no header), so both the single-terminal and split-pane paths agree here.
/// Whether a blinking text cursor is on screen right now — i.e. the terminal has
/// keyboard focus and the tab is a terminal (RDP draws the remote's own cursor).
/// Gates both the blink timer and the painting, so they can't disagree.
pub(crate) fn terminal_cursor_blinks(app: &AditApp) -> bool {
    app.terminal_focused && !app.manager.active_is_rdp() && app.manager.active_session().is_some()
}

pub(crate) fn window_to_pane_local(app: &AditApp, position: Point) -> Point {
    let index = if app.panes.is_empty() {
        0
    } else {
        app.focused_pane.min(app.panes.len() - 1)
    };
    let origin = pane_layout(app).pane_body_origin(index);
    Point::new(position.x - origin.x, position.y - origin.y)
}

/// Map a window-absolute cursor Y (during a scrollbar drag) to a scrollback
/// offset. The scrollbar spans the focused pane's terminal body; the thumb centre
/// follows the cursor, top = oldest (max offset), bottom = newest (offset 0).
pub(crate) fn scrollbar_drag_to(app: &mut AditApp, window_y: f32) {
    let max_offset = max_terminal_scroll_offset(app);
    if max_offset == 0 {
        return;
    }
    let index = if app.panes.is_empty() {
        0
    } else {
        app.focused_pane.min(app.panes.len().saturating_sub(1))
    };
    let layout = pane_layout(app);
    let origin = layout.pane_body_origin(index);
    // The scrollbar track = the lines area (pane body minus the panel padding).
    let track_top = origin.y + TERMINAL_PANEL_PADDING;
    let track_h = (layout.pane_h - layout.header - TERMINAL_PANEL_PADDING * 2.0).max(1.0);
    // 0 at the top of the track, 1 at the bottom.
    let frac = ((window_y - track_top) / track_h).clamp(0.0, 1.0);
    // Bottom (frac 1) = newest = offset 0; top (frac 0) = oldest = max offset.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let offset = ((1.0 - frac) * max_offset as f32).round() as usize;
    app.terminal_scroll_offset = offset.min(max_offset);
}

/// While a scrollbar drag is live, track the cursor and the button-up GLOBALLY, so
/// the drag survives leaving the thin scrollbar (same as the sidebar resize).
pub(crate) fn scrollbar_drag_event(
    event: event::Event,
    _status: event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        event::Event::Mouse(mouse::Event::CursorMoved { position }) => {
            Some(Message::ScrollbarDragMove(position.y))
        }
        event::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
            Some(Message::EndScrollbarDrag)
        }
        _ => None,
    }
}

/// Extend a live drag-selection to the cursor, and arm/disarm auto-scroll when the
/// cursor is dragged past the pane's top/bottom edge.
pub(crate) fn extend_terminal_selection(app: &mut AditApp, point: Point) {
    let end = terminal_selection_point(app, point);
    let overshoot = terminal_row_overshoot(app, point);
    if let Some(selection) = &mut app.terminal_selection {
        selection.end = end;
    }
    // Scroll faster the further past the edge the cursor is, but stay smooth.
    app.selection_autoscroll = overshoot.signum() * overshoot.abs().min(5);
}

/// One auto-scroll step while drag-selecting past an edge: scroll, then re-anchor
/// the selection end to the edge row now under the cursor.
pub(crate) fn selection_autoscroll_tick(app: &mut AditApp) {
    let lines = app.selection_autoscroll;
    if lines == 0 || !app.terminal_selecting {
        return;
    }
    // Past the bottom ⇒ reveal newer rows (offset down); past the top ⇒ older.
    let before = app.terminal_scroll_offset;
    apply_terminal_scroll(app, TerminalScrollAction::Lines(-lines));
    if app.terminal_scroll_offset == before {
        // Already at the end of the scrollback; nothing more to reveal.
        return;
    }

    let rows = terminal_view_rows(app);
    let first_row = viewport_first_row(app);
    let edge_row = if lines > 0 {
        first_row + rows.saturating_sub(1)
    } else {
        first_row
    };
    if let Some(selection) = &mut app.terminal_selection {
        selection.end.row = edge_row;
    }
}

/// Select the whole word under `point` (double-click).
pub(crate) fn select_word_at(app: &mut AditApp, point: TerminalPoint) {
    let snapshot = active_terminal_snapshot(app);
    let line = snapshot_line_text(&snapshot, point.row);
    app.terminal_selection = word_bounds(&line, point.col).map(|(start, end)| TerminalSelection {
        start: TerminalPoint {
            row: point.row,
            col: start,
        },
        end: TerminalPoint {
            row: point.row,
            col: end,
        },
    });
}

/// Select the entire (trailing-blank-trimmed) line under `point` (triple-click).
pub(crate) fn select_line_at(app: &mut AditApp, point: TerminalPoint) {
    let snapshot = active_terminal_snapshot(app);
    let line = snapshot_line_text(&snapshot, point.row);
    let len = line.trim_end().chars().count();
    app.terminal_selection = Some(TerminalSelection {
        start: TerminalPoint {
            row: point.row,
            col: 0,
        },
        end: TerminalPoint {
            row: point.row,
            col: len,
        },
    });
}

/// Text of an ABSOLUTE scrollback row, resolved against the snapshot's window.
/// Word span `[start, end)` (char indices) around `col` for double-click select.
/// "Word" chars are alphanumerics plus the punctuation that commonly appears
/// mid-token in paths, URLs, and hostnames, so `/usr/local/bin` stays one word.
/// The absolute scrollback row currently shown at the top of the viewport.
/// Mirrors the window `active_terminal_snapshot` renders.
pub(crate) fn viewport_first_row(app: &AditApp) -> usize {
    let rows = terminal_view_rows(app);
    let tail = app.manager.active_snapshot(Viewport::tail(rows));
    let offset = app
        .terminal_scroll_offset
        .min(max_scroll_offset_for(&tail, rows));
    tail.total_rows.saturating_sub(rows).saturating_sub(offset)
}

/// The cursor's position as an ABSOLUTE scrollback point, for anchoring a
/// selection. (`terminal_point_from_cursor` stays viewport-relative because mouse
/// reporting must send viewport cells to the remote app.)
pub(crate) fn terminal_selection_point(app: &AditApp, point: Point) -> TerminalPoint {
    let viewport = terminal_point_from_cursor(app, point);
    TerminalPoint {
        row: viewport_first_row(app).saturating_add(viewport.row),
        col: viewport.col,
    }
}

/// How many rows the cursor is past the viewport edge: negative above the top,
/// positive below the bottom, 0 inside. Drives selection auto-scroll.
pub(crate) fn terminal_row_overshoot(app: &AditApp, point: Point) -> i32 {
    let origin_y = TERMINAL_PANEL_PADDING + TERMINAL_HEADER_AND_GAP;
    let raw = ((point.y - origin_y) / cell_height()).floor();
    let last = f32::from(app.terminal_size.rows.saturating_sub(1));
    let overshoot = if raw < 0.0 {
        raw
    } else if raw > last {
        raw - last
    } else {
        0.0
    };
    (overshoot as i32).clamp(-40, 40)
}

/// Map an absolute-row selection into viewport row space for rendering, clipping
/// to the visible window. `None` when the selection is entirely off-screen. A row
/// clipped off the top starts at column 0; one clipped off the bottom runs to EOL
/// (`usize::MAX`), matching `selection_range_for_row`'s convention.
pub(crate) fn terminal_point_from_cursor(app: &AditApp, point: Point) -> TerminalPoint {
    // `point` comes from the terminal panel's own `mouse_area` (`on_move` uses
    // `cursor.position_in(bounds)`), so it is already relative to the panel's
    // top-left. Only the panel's internal padding offsets the text grid; the
    // window chrome does not, and the context menu now floats (no shift).
    let origin_x = TERMINAL_PANEL_PADDING;
    let origin_y = TERMINAL_PANEL_PADDING + TERMINAL_HEADER_AND_GAP;

    let col = ((point.x - origin_x) / cell_width()).floor().max(0.0) as usize;
    let row = ((point.y - origin_y) / cell_height()).floor().max(0.0) as usize;

    TerminalPoint {
        row: row.min(usize::from(app.terminal_size.rows.saturating_sub(1))),
        col: col.min(usize::from(app.terminal_size.cols)),
    }
}

/// Map an iced keyboard event to an RDP scancode press/release.
/// Returns `(scancode, extended, pressed)` using PC/AT set-1 make codes; the
/// `extended` flag marks the E0 set (right-side modifiers, nav cluster, arrows,
/// numpad Enter/Divide, Windows/Menu keys). Layout-independent: it keys off the
/// physical key, so the remote applies its own layout.
/// PC/AT set-1 make code + E0-extended flag for a physical key code.
pub(crate) fn rdp_scancode_for_code(code: keyboard::key::Code) -> Option<(u8, bool)> {
    use keyboard::key::Code::*;
    let mapped = match code {
        Escape => (0x01, false),
        Digit1 => (0x02, false),
        Digit2 => (0x03, false),
        Digit3 => (0x04, false),
        Digit4 => (0x05, false),
        Digit5 => (0x06, false),
        Digit6 => (0x07, false),
        Digit7 => (0x08, false),
        Digit8 => (0x09, false),
        Digit9 => (0x0A, false),
        Digit0 => (0x0B, false),
        Minus => (0x0C, false),
        Equal => (0x0D, false),
        Backspace => (0x0E, false),
        Tab => (0x0F, false),
        KeyQ => (0x10, false),
        KeyW => (0x11, false),
        KeyE => (0x12, false),
        KeyR => (0x13, false),
        KeyT => (0x14, false),
        KeyY => (0x15, false),
        KeyU => (0x16, false),
        KeyI => (0x17, false),
        KeyO => (0x18, false),
        KeyP => (0x19, false),
        BracketLeft => (0x1A, false),
        BracketRight => (0x1B, false),
        Enter => (0x1C, false),
        ControlLeft => (0x1D, false),
        KeyA => (0x1E, false),
        KeyS => (0x1F, false),
        KeyD => (0x20, false),
        KeyF => (0x21, false),
        KeyG => (0x22, false),
        KeyH => (0x23, false),
        KeyJ => (0x24, false),
        KeyK => (0x25, false),
        KeyL => (0x26, false),
        Semicolon => (0x27, false),
        Quote => (0x28, false),
        Backquote => (0x29, false),
        ShiftLeft => (0x2A, false),
        Backslash => (0x2B, false),
        KeyZ => (0x2C, false),
        KeyX => (0x2D, false),
        KeyC => (0x2E, false),
        KeyV => (0x2F, false),
        KeyB => (0x30, false),
        KeyN => (0x31, false),
        KeyM => (0x32, false),
        Comma => (0x33, false),
        Period => (0x34, false),
        Slash => (0x35, false),
        ShiftRight => (0x36, false),
        NumpadMultiply => (0x37, false),
        AltLeft => (0x38, false),
        Space => (0x39, false),
        CapsLock => (0x3A, false),
        F1 => (0x3B, false),
        F2 => (0x3C, false),
        F3 => (0x3D, false),
        F4 => (0x3E, false),
        F5 => (0x3F, false),
        F6 => (0x40, false),
        F7 => (0x41, false),
        F8 => (0x42, false),
        F9 => (0x43, false),
        F10 => (0x44, false),
        NumLock => (0x45, false),
        ScrollLock => (0x46, false),
        Numpad7 => (0x47, false),
        Numpad8 => (0x48, false),
        Numpad9 => (0x49, false),
        NumpadSubtract => (0x4A, false),
        Numpad4 => (0x4B, false),
        Numpad5 => (0x4C, false),
        Numpad6 => (0x4D, false),
        NumpadAdd => (0x4E, false),
        Numpad1 => (0x4F, false),
        Numpad2 => (0x50, false),
        Numpad3 => (0x51, false),
        Numpad0 => (0x52, false),
        NumpadDecimal => (0x53, false),
        IntlBackslash => (0x56, false),
        F11 => (0x57, false),
        F12 => (0x58, false),
        // ── E0-extended set ──────────────────────────────────────────────
        NumpadEnter => (0x1C, true),
        ControlRight => (0x1D, true),
        NumpadDivide => (0x35, true),
        AltRight => (0x38, true),
        Home => (0x47, true),
        ArrowUp => (0x48, true),
        PageUp => (0x49, true),
        ArrowLeft => (0x4B, true),
        ArrowRight => (0x4D, true),
        End => (0x4F, true),
        ArrowDown => (0x50, true),
        PageDown => (0x51, true),
        Insert => (0x52, true),
        Delete => (0x53, true),
        SuperLeft => (0x5B, true),
        SuperRight => (0x5C, true),
        ContextMenu => (0x5D, true),
        _ => return None,
    };
    Some(mapped)
}

pub(crate) fn encode_keyboard_event(event: keyboard::Event) -> Option<Vec<u8>> {
    let keyboard::Event::KeyPressed {
        key,
        modified_key,
        physical_key,
        modifiers,
        text,
        ..
    } = event
    else {
        return None;
    };

    if modifiers.control() {
        if let Some(byte) = control_byte(&key, physical_key) {
            return Some(vec![byte]);
        }
    }

    if let Key::Named(named) = key {
        if let Some(sequence) = named_key_sequence(named, modifiers) {
            return Some(sequence.as_bytes().to_vec());
        }
    }

    if modifiers.control() {
        return None;
    }

    if let Some(text) = text {
        if !text.is_empty() {
            let mut bytes = Vec::new();
            if modifiers.alt() {
                bytes.push(0x1b);
            }
            bytes.extend_from_slice(text.as_bytes());
            return Some(bytes);
        }
    }

    if let Key::Character(character) = modified_key.as_ref() {
        if !character.is_empty() {
            let mut bytes = Vec::new();
            if modifiers.alt() {
                bytes.push(0x1b);
            }
            bytes.extend_from_slice(character.as_bytes());
            return Some(bytes);
        }
    }

    None
}

pub(crate) fn control_byte(key: &Key, physical_key: keyboard::key::Physical) -> Option<u8> {
    let character = key
        .to_latin(physical_key)
        .or_else(|| match key.as_ref() {
            Key::Character(text) => text.chars().next(),
            _ => None,
        })?
        .to_ascii_lowercase();

    match character {
        'a'..='z' => Some((character as u8) - b'a' + 1),
        ' ' | '@' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

pub(crate) fn named_key_sequence(named: Named, modifiers: keyboard::Modifiers) -> Option<&'static str> {
    match named {
        Named::Enter => Some("\r"),
        Named::Tab if modifiers.shift() => Some("\x1b[Z"),
        Named::Tab => Some("\t"),
        Named::Backspace => Some("\x7f"),
        Named::Escape => Some("\x1b"),
        Named::ArrowUp => Some("\x1b[A"),
        Named::ArrowDown => Some("\x1b[B"),
        Named::ArrowRight => Some("\x1b[C"),
        Named::ArrowLeft => Some("\x1b[D"),
        Named::Home => Some("\x1b[H"),
        Named::End => Some("\x1b[F"),
        Named::Insert => Some("\x1b[2~"),
        Named::Delete => Some("\x1b[3~"),
        Named::PageUp => Some("\x1b[5~"),
        Named::PageDown => Some("\x1b[6~"),
        Named::F1 => Some("\x1bOP"),
        Named::F2 => Some("\x1bOQ"),
        Named::F3 => Some("\x1bOR"),
        Named::F4 => Some("\x1bOS"),
        Named::F5 => Some("\x1b[15~"),
        Named::F6 => Some("\x1b[17~"),
        Named::F7 => Some("\x1b[18~"),
        Named::F8 => Some("\x1b[19~"),
        Named::F9 => Some("\x1b[20~"),
        Named::F10 => Some("\x1b[21~"),
        Named::F11 => Some("\x1b[23~"),
        Named::F12 => Some("\x1b[24~"),
        _ => None,
    }
}

pub(crate) fn clear_active_terminal(app: &mut AditApp) {
    match app.manager.clear_active_terminal() {
        Ok(()) => {
            app.terminal_scroll_offset = 0;
            app.terminal_selection = None;
            app.terminal_context_menu = false;
            app.last_error = None;
            app.notice = String::from(t("当前终端已清屏"));
        }
        Err(error) => app.last_error = Some(error.to_string()),
    }
}

pub(crate) fn toggle_active_logging(app: &mut AditApp) {
    let Some(summary) = app.manager.active_session_summary() else {
        app.last_error = Some(String::from(t("没有活动会话")));
        return;
    };

    if app.manager.active_is_logging() {
        if let Some(path) = app.manager.stop_active_logging() {
            app.notice = format!("已停止记录会话日志: {}", path.display());
        }
    } else {
        let dir = effective_log_dir(app);
        let name = render_log_name(&effective_log_pattern(app), &summary.title, &summary.endpoint);
        match app.manager.start_active_logging(&dir, &name, app.log_plaintext) {
            Ok(path) => {
                app.last_error = None;
                app.notice = format!("正在记录会话输出到: {}", path.display());
            }
            Err(error) => app.last_error = Some(tf("开启会话日志失败: {}", &[&error])),
        }
    }
}

/// Default session-log filename pattern (SecureCRT-style tokens).
pub(crate) const DEFAULT_LOG_PATTERN: &str = "%N_%Y-%M-%D_%h-%m-%s.log";

/// The effective log folder: the user's override, else `logs/` under the config
/// folder in use this run. Pinned to `app.config_dir` (not the live
/// `config_dir()`), so a pending config relocation never splits logs off to a
/// half-applied location before restart.
pub(crate) fn effective_log_dir(app: &AditApp) -> std::path::PathBuf {
    if app.log_dir.trim().is_empty() {
        app.config_dir.join("logs")
    } else {
        std::path::PathBuf::from(app.log_dir.trim())
    }
}

pub(crate) fn effective_log_pattern(app: &AditApp) -> String {
    if app.log_name_pattern.trim().is_empty() {
        DEFAULT_LOG_PATTERN.to_string()
    } else {
        app.log_name_pattern.clone()
    }
}

/// Current local time broken into (year, month, day, hour, minute, second).
pub(crate) fn now_local_parts() -> (i32, u8, u8, u8, u8, u8) {
    let offset = time::UtcOffset::from_whole_seconds(local_offset_secs() as i32)
        .unwrap_or(time::UtcOffset::UTC);
    let now = time::OffsetDateTime::now_utc().to_offset(offset);
    (
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    )
}

/// Render a log-filename pattern: `%N` session name, `%H` host, `%Y/%M/%D` date,
/// `%h/%m/%s` time. The host is parsed from the session endpoint.
pub(crate) fn render_log_name(pattern: &str, session_name: &str, endpoint: &str) -> String {
    let host = endpoint
        .rsplit_once('@')
        .map(|(_, rest)| rest)
        .unwrap_or(endpoint);
    let host = host.split(':').next().unwrap_or(host);
    let (y, mo, d, h, mi, s) = now_local_parts();
    pattern
        .replace("%N", session_name)
        .replace("%H", host)
        .replace("%Y", &format!("{y:04}"))
        .replace("%M", &format!("{mo:02}"))
        .replace("%D", &format!("{d:02}"))
        .replace("%h", &format!("{h:02}"))
        .replace("%m", &format!("{mi:02}"))
        .replace("%s", &format!("{s:02}"))
}

/// Open a folder in the OS file manager (creating it first if missing).
pub(crate) fn open_folder(app: &mut AditApp, dir: std::path::PathBuf) {
    if let Err(error) = std::fs::create_dir_all(&dir) {
        app.last_error = Some(format!("无法创建目录 {}: {error}", dir.display()));
        return;
    }
    let opener = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    // explorer.exe returns a nonzero exit code even on success, so ignore the
    // status and only surface a spawn failure.
    match std::process::Command::new(opener).arg(&dir).spawn() {
        Ok(_) => app.notice = format!("已在文件管理器中打开: {}", dir.display()),
        Err(error) => app.last_error = Some(tf("打开目录失败: {}", &[&error])),
    }
}

/// Start logging any freshly-connected sessions when auto-log is enabled.
pub(crate) fn auto_log_connected_sessions(app: &mut AditApp) {
    if !app.auto_log_on_connect {
        return;
    }
    let dir = effective_log_dir(app);
    let pattern = effective_log_pattern(app);
    let targets: Vec<(SessionId, String, String)> = app
        .manager
        .sessions()
        .into_iter()
        .filter(|summary| summary.status == SessionStatus::Connected)
        .filter(|summary| !app.manager.session_is_logging(summary.id))
        .map(|summary| (summary.id, summary.title, summary.endpoint))
        .collect();

    let plaintext = app.log_plaintext;
    for (session_id, title, endpoint) in targets {
        let name = render_log_name(&pattern, &title, &endpoint);
        if let Err(error) = app.manager.start_logging(session_id, &dir, &name, plaintext) {
            app.last_error = Some(tf("自动日志开启失败: {}", &[&error]));
        }
    }
}

pub(crate) fn resize_active(app: &mut AditApp, cols: u16, rows: u16) {
    match app.manager.resize_active(cols, rows) {
        Ok(()) => {
            app.terminal_size = TerminalSize::new(cols, rows);
            app.last_error = None;
            app.notice = tf("当前终端尺寸已设置为 {}x{}", &[&cols, &rows]);
        }
        Err(error) => app.last_error = Some(error.to_string()),
    }
}

/// Pixel size of the whole terminal region (the grid the panes share), i.e. the
/// workspace minus the sidebar, top chrome, tab bar, and status bar. Pane
/// padding/headers are *not* subtracted here — that happens per pane.
pub(crate) fn terminal_region_area(
    width: f32,
    height: f32,
    sidebar_width: f32,
    fullscreen: bool,
) -> (f32, f32) {
    let region_width = (width - sidebar_width).max(0.0);
    // Fullscreen drops the menu bar and the toolbar; the tab strip and the
    // status bar stay. Subtracting chrome that is not on screen makes the
    // requested remote desktop smaller than the space it has to fill, and the
    // remainder shows as black bars down the right and bottom edges.
    let chrome = if fullscreen {
        TAB_BAR_HEIGHT + STATUS_BAR_HEIGHT
    } else {
        MENU_BAR_HEIGHT + TAB_BAR_HEIGHT + STATUS_BAR_HEIGHT
    };
    let region_height = (height - chrome).max(0.0);
    (region_width, region_height)
}

/// The RDP remote-desktop size to request: the on-screen surface area, so the
/// framebuffer maps ~1:1 to the widget instead of being upscaled from a fixed
/// 1280×720 (which blurs). Rounded to even dimensions (RDP dislikes odd) and
/// clamped to sane bounds. Returns 0×0 when there's no room yet (caller falls
/// back to the helper's default).
pub(crate) fn rdp_viewport_size(app: &AditApp) -> (u16, u16) {
    // The divider sits between the sidebar and the workspace and is not part
    // of either; forgetting it made the frame ~5 logical px wider than the
    // pane, so ContentFit had to downscale by ~0.997 — enough to shimmer text.
    let sidebar = if app.sidebar_visible && !app.fullscreen {
        app.sidebar_width + SIDEBAR_DIVIDER_WIDTH
    } else {
        0.0
    };
    let (w, h) = terminal_region_area(app.window_width, app.window_height, sidebar, app.fullscreen);
    // PHYSICAL pixels: the window lays out in logical points, but the remote
    // desktop is a pixel grid. Requesting logical sizes meant a 125%-DPI
    // display got a 1524x920 desktop stretched over ~1905x1150 device pixels —
    // never 1:1, so text was resampled no matter what the codec did.
    let scale = app.display_scale.max(0.1);
    let (w, h) = (w * scale, h * scale);
    // Round down to a multiple of 4 — some RDP servers reject odd/non-aligned
    // desktop dimensions.
    let align4 = |v: f32| ((v.round() as i32).clamp(0, 8192) as u16) & !3;
    let (w, h) = (align4(w), align4(h));
    // Below the RDP minimum (200×200) there's effectively no viewport; signal
    // "use default" with 0×0 rather than requesting a degenerate desktop.
    if w < 200 || h < 200 {
        (0, 0)
    } else {
        (w, h)
    }
}

/// After a layout change, ask the active RDP desktop to match the viewport, but
/// only when the target actually changed (window/sidebar drags fire continuously).
pub(crate) fn maybe_resize_active_rdp(app: &mut AditApp) {
    if !app.manager.active_rdp_live() {
        return;
    }
    let (w, h) = rdp_viewport_size(app);
    if w == 0 || h == 0 || app.rdp_target_size == Some((w, h)) {
        return;
    }
    app.rdp_target_size = Some((w, h));
    app.manager.resize_active_rdp(w, h);
}

/// Cols/rows that fit in a single pane's *inner* pixel area (after its own
/// padding + header have already been removed by the caller).
pub(crate) fn terminal_size_for_area(inner_width: f32, inner_height: f32) -> TerminalSize {
    let cols = (inner_width / cell_width()).floor().clamp(20.0, 220.0) as u16;
    let rows = (inner_height / cell_height()).floor().clamp(6.0, 80.0) as u16;
    TerminalSize::new(cols, rows)
}

/// How many columns × rows of panes a given pane count tiles into.
/// How split panes are arranged (SecureCRT-style tiling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum TileMode {
    /// A roughly square grid (default; 2 across, 2×2 at four, etc.).
    #[default]
    Grid,
    /// All side by side in one row (vertical tiling).
    Columns,
    /// All stacked in one column (horizontal tiling).
    Rows,
}

/// Where the command window sends a typed line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CommandTarget {
    /// Only the active session/tab.
    #[default]
    ActiveSession,
    /// Every connected session at once (broadcast).
    AllSessions,
}

impl CommandTarget {
    pub(crate) fn label(self) -> &'static str {
        match self {
            CommandTarget::ActiveSession => "当前会话",
            CommandTarget::AllSessions => "所有会话",
        }
    }

    pub(crate) fn toggled(self) -> Self {
        match self {
            CommandTarget::ActiveSession => CommandTarget::AllSessions,
            CommandTarget::AllSessions => CommandTarget::ActiveSession,
        }
    }
}

pub(crate) fn pane_grid_dims(count: usize, mode: TileMode) -> (usize, usize) {
    let count = count.max(1);
    match mode {
        TileMode::Columns => (count, 1),
        TileMode::Rows => (1, count),
        TileMode::Grid => match count {
            0 | 1 => (1, 1),
            2 => (2, 1),
            3 => (3, 1),
            4 => (2, 2),
            5 | 6 => (3, 2),
            _ => {
                let cols = (count as f32).sqrt().ceil() as usize;
                (cols, count.div_ceil(cols))
            }
        },
    }
}

/// Geometry for the current pane layout: grid shape, per-pane outer pixel size,
/// the terminal region's screen origin, and the header height (0 when single).
pub(crate) struct PaneLayout {
    pub(crate) cols: usize,
    pub(crate) pane_w: f32,
    pub(crate) pane_h: f32,
    pub(crate) origin_x: f32,
    pub(crate) origin_y: f32,
    pub(crate) header: f32,
}

pub(crate) fn pane_layout(app: &AditApp) -> PaneLayout {
    let effective_sidebar = if app.sidebar_visible && !app.fullscreen {
        app.sidebar_width + SIDEBAR_DIVIDER_WIDTH
    } else {
        0.0
    };
    let (region_w, region_h) = terminal_region_area(
        app.window_width,
        app.window_height,
        effective_sidebar,
        app.fullscreen,
    );

    let count = app.panes.len().max(1);
    let (cols, rows) = pane_grid_dims(count, app.tile_mode);
    let header = if count > 1 { PANE_HEADER_HEIGHT } else { 0.0 };

    let pane_w = ((region_w - PANE_GAP * (cols as f32 - 1.0)) / cols as f32).max(1.0);
    let pane_h = ((region_h - PANE_GAP * (rows as f32 - 1.0)) / rows as f32).max(1.0);

    PaneLayout {
        cols,
        pane_w,
        pane_h,
        origin_x: effective_sidebar,
        origin_y: MENU_BAR_HEIGHT + TAB_BAR_HEIGHT,
        header,
    }
}

impl PaneLayout {
    /// Cols/rows that fit one pane in this layout.
    pub(crate) fn pane_terminal_size(&self) -> TerminalSize {
        // The scrollbar gutter eats into the text width, so the grid must fit the
        // remaining space — otherwise the remote wraps a column or two past the
        // visible edge.
        let inner_w = self.pane_w - TERMINAL_PANEL_PADDING * 2.0 - SCROLLBAR_WIDTH;
        let inner_h = self.pane_h - self.header - TERMINAL_PANEL_PADDING * 2.0;
        terminal_size_for_area(inner_w, inner_h)
    }

    /// Screen-space top-left of a pane's terminal *body* (below its header).
    pub(crate) fn pane_body_origin(&self, index: usize) -> Point {
        let gc = index % self.cols;
        let gr = index / self.cols;
        Point::new(
            self.origin_x + gc as f32 * (self.pane_w + PANE_GAP),
            self.origin_y + gr as f32 * (self.pane_h + PANE_GAP) + self.header,
        )
    }
}

/// Single-pane / no-split terminal size, for the common path and the status bar.
pub(crate) fn estimated_terminal_size(
    width: f32,
    height: f32,
    sidebar_width: f32,
    fullscreen: bool,
) -> TerminalSize {
    let (region_w, region_h) = terminal_region_area(width, height, sidebar_width, fullscreen);
    terminal_size_for_area(
        region_w - TERMINAL_PANEL_PADDING * 2.0 - SCROLLBAR_WIDTH,
        region_h - TERMINAL_PANEL_PADDING * 2.0,
    )
}

/// Adjust the terminal font size by `delta` px, clamped to the sane range, and
/// re-fit the grid. Shared by the appearance dialog's +/- and Ctrl+wheel zoom.
pub(crate) fn step_font_size(app: &mut AditApp, delta: i32) {
    let next = (app.font_size as i32 + delta).clamp(MIN_FONT_SIZE as i32, MAX_FONT_SIZE as i32);
    app.font_size = next as f32;
    sync_terminal_size(app);
}

pub(crate) fn sync_terminal_size(app: &mut AditApp) {
    // An RDP desktop tracks the viewport in pixels, which can change even when the
    // terminal's cols/rows (below) don't — so do this before the grid early-return.
    maybe_resize_active_rdp(app);

    let layout = pane_layout(app);
    let target = layout.pane_terminal_size();

    // Keep the manager's "new session" size current even when nothing needs
    // resizing, so a freshly-connected shell's PTY matches the visible width
    // (otherwise its ls/output would wrap for the old default width).
    app.manager
        .set_default_terminal_size(target.cols, target.rows);

    // Skip the common no-change case so a window drag does not spam resizes.
    // Pane add/close changes the pane count → the per-pane target changes, so
    // this still fits panes on split/unsplit; a same-count session *swap* fits
    // the swapped-in session explicitly in the ActivateSession handler.
    if target == app.terminal_size {
        return;
    }

    // A width change reflows the grid, which renumbers every absolute scrollback
    // row. The selection is anchored in that numbering and the scroll offset is
    // counted against the old row total, so both would point at different text
    // afterwards. Anchor them to logical lines — the pre-wrap unit, which a
    // re-wrap preserves — and resolve them back once the resize has landed. A
    // height change re-wraps nothing and leaves the numbering intact.
    let reflowing = target.cols != app.terminal_size.cols;
    let anchors = reflowing.then(|| capture_reflow_anchors(app));

    app.terminal_size = target;

    if app.panes.is_empty() {
        if app.manager.active_session().is_some() {
            if let Err(error) = app.manager.resize_active(target.cols, target.rows) {
                app.last_error = Some(error.to_string());
            }
        }
    } else {
        for &session_id in &app.panes {
            if let Err(error) = app.manager.resize_session(session_id, target.cols, target.rows) {
                app.last_error = Some(error.to_string());
            }
        }
    }

    if let Some(anchors) = anchors {
        restore_reflow_anchors(app, &anchors);
    }
}

/// The selection ends and the scrolled-to top row, anchored to logical lines so
/// they survive the re-wrap a width change triggers. The top row goes last, so
/// the two halves stay in step with `restore_reflow_anchors`.
pub(crate) fn capture_reflow_anchors(app: &AditApp) -> Vec<LogicalAnchor> {
    let mut points = Vec::with_capacity(3);
    if let Some(selection) = app.terminal_selection {
        points.push((selection.start.row, selection.start.col));
        points.push((selection.end.row, selection.end.col));
    }
    points.push((viewport_first_row(app), 0));
    app.manager.anchor_active_points(&points)
}

/// Put the selection and the scroll position back on the text they were on,
/// at the rows the re-wrap moved it to.
pub(crate) fn restore_reflow_anchors(app: &mut AditApp, anchors: &[LogicalAnchor]) {
    let resolved = app.manager.resolve_active_anchors(anchors);
    let Some((&(top_row, _), ends)) = resolved.split_last() else {
        return;
    };

    if let (Some(selection), [(start_row, start_col), (end_row, end_col)]) =
        (&mut app.terminal_selection, ends)
    {
        selection.start = TerminalPoint {
            row: *start_row,
            col: *start_col,
        };
        selection.end = TerminalPoint {
            row: *end_row,
            col: *end_col,
        };
    }

    // Offset 0 means "follow the output", not "the row that happened to be at the
    // top" — re-deriving one there would unpin the view from the newest line.
    if app.terminal_scroll_offset > 0 {
        // The offset counts rows up from the bottom, and the re-wrap changed the
        // total, so it has to be re-derived from the row rather than kept.
        app.terminal_scroll_offset = max_terminal_scroll_offset(app).saturating_sub(top_row);
    }
}

/// Add another connected session as a split pane (up to [`MAX_PANES`]).
/// Tile every open session (up to [`MAX_PANES`]) in the given orientation —
/// SecureCRT-style Tile Vertically / Horizontally / grid.
pub(crate) fn tile_all_sessions(app: &mut AditApp, mode: TileMode) {
    let ids: Vec<SessionId> = app
        .manager
        .sessions()
        .into_iter()
        .map(|summary| summary.id)
        .take(MAX_PANES)
        .collect();
    if ids.len() < 2 {
        app.notice = String::from(t("至少要两个会话才能平铺（先多连接/打开几个会话）"));
        return;
    }

    app.panes = ids;
    app.tile_mode = mode;
    // Keep the active session focused if it is among the tiled panes.
    if let Some(active) = app.manager.active_session() {
        if let Some(pos) = app.panes.iter().position(|id| *id == active) {
            app.focused_pane = pos;
        }
    }
    app.focused_pane = app.focused_pane.min(app.panes.len() - 1);
    app.terminal_focused = true;
    app.terminal_scroll_offset = 0;
    app.terminal_selection = None;
    app.terminal_context_menu = false;
    sync_terminal_size(app);
    let label = match mode {
        TileMode::Columns => "垂直",
        TileMode::Rows => "水平",
        TileMode::Grid => "网格",
    };
    app.notice = format!("已{label}平铺 {} 个会话", app.panes.len());
}

/// Collapse split panes back to the single-pane tabbed view.
pub(crate) fn untile_sessions(app: &mut AditApp) {
    app.panes.clear();
    app.focused_pane = 0;
    app.terminal_scroll_offset = 0;
    app.terminal_selection = None;
    sync_terminal_size(app);
    app.notice = String::from(t("已合并为单标签视图"));
}

pub(crate) fn split_pane(app: &mut AditApp) {
    // RDP is a full-surface graphical session and isn't rendered inside a tile, so
    // it can't participate in split panes.
    if app.panes.is_empty() && app.manager.active_is_rdp() {
        app.last_error = Some(String::from(t("RDP 会话不支持分屏")));
        return;
    }

    app.tile_mode = TileMode::Grid;
    // Seed the tiling from the active session on the first split.
    if app.panes.is_empty() {
        match app.manager.active_session() {
            Some(active) => {
                app.panes.push(active);
                app.focused_pane = 0;
            }
            None => {
                app.last_error = Some(String::from(t("请先连接一个会话再分屏")));
                return;
            }
        }
    }

    if app.panes.len() >= MAX_PANES {
        app.notice = tf("最多同时分屏 {} 个终端", &[&MAX_PANES]);
        return;
    }

    // First open session not already shown in a pane. RDP sessions are excluded —
    // they render as a full surface, not a terminal tile.
    let candidate = app
        .manager
        .sessions()
        .into_iter()
        .map(|summary| summary.id)
        .find(|id| !app.panes.contains(id) && !app.manager.session_is_rdp(*id));

    let Some(session_id) = candidate else {
        app.panes.clear();
        app.focused_pane = 0;
        app.notice = String::from(t("没有更多会话可分屏（先在侧栏连接另一个会话）"));
        return;
    };

    let insert_at = (app.focused_pane + 1).min(app.panes.len());
    app.panes.insert(insert_at, session_id);
    app.focused_pane = insert_at;
    let _ = app.manager.activate(session_id);
    app.terminal_focused = true;
    app.terminal_scroll_offset = 0;
    app.terminal_selection = None;
    app.terminal_context_menu = false;
    sync_terminal_size(app);
    app.notice = format!("已分屏：{} 个终端并排", app.panes.len());
}

/// Remove a pane from the tiling (does not close the session). Collapses back to
/// the single-pane view when one or fewer remain.
pub(crate) fn close_pane(app: &mut AditApp, index: usize) {
    if index >= app.panes.len() {
        return;
    }
    app.panes.remove(index);

    if app.panes.len() <= 1 {
        let remaining = app.panes.first().copied();
        app.panes.clear();
        app.focused_pane = 0;
        if let Some(session_id) = remaining {
            let _ = app.manager.activate(session_id);
        }
    } else {
        if index < app.focused_pane || app.focused_pane >= app.panes.len() {
            app.focused_pane = app
                .focused_pane
                .saturating_sub(1)
                .min(app.panes.len() - 1);
        }
        if let Some(&session_id) = app.panes.get(app.focused_pane) {
            let _ = app.manager.activate(session_id);
        }
    }

    app.terminal_scroll_offset = 0;
    app.terminal_selection = None;
    app.terminal_context_menu = false;
    sync_terminal_size(app);
}

/// Activate a session (from a tab click). In a split, load it into the focused
/// pane instead of collapsing the layout.
pub(crate) fn activate_session(app: &mut AditApp, session_id: SessionId) {
    // Switching sessions counts as clicking away from an in-place rename.
    commit_inline_rename(app);
    if app.panes.len() >= 2 {
        if let Some(pos) = app.panes.iter().position(|id| *id == session_id) {
            app.focused_pane = pos;
        } else if app.focused_pane < app.panes.len() {
            app.panes[app.focused_pane] = session_id;
            let _ = app.manager.resize_session(
                session_id,
                app.terminal_size.cols,
                app.terminal_size.rows,
            );
        }
    }
    if let Err(error) = app.manager.activate(session_id) {
        app.last_error = Some(error.to_string());
    } else {
        app.terminal_focused = true;
        app.terminal_scroll_offset = 0;
        app.terminal_selection = None;
        app.terminal_context_menu = false;
        sync_terminal_size(app);
    }
}

/// Focus a pane: make its session active and reset per-pane scroll/selection.
pub(crate) fn focus_pane(app: &mut AditApp, index: usize) {
    let Some(&session_id) = app.panes.get(index) else {
        return;
    };
    let changed =
        app.focused_pane != index || app.manager.active_session() != Some(session_id);
    app.focused_pane = index;
    if app.manager.active_session() != Some(session_id) {
        let _ = app.manager.activate(session_id);
    }
    app.terminal_focused = true;
    app.terminal_context_menu = false;
    if changed {
        app.terminal_scroll_offset = 0;
        app.terminal_selection = None;
    }
}

/// Keep `panes`/`focused_pane` consistent with the live session set: drop closed
/// sessions and duplicates, and collapse to the single-pane view when the split
/// no longer makes sense (≤1 pane, or the active session is not tiled).
pub(crate) fn sync_panes(app: &mut AditApp) {
    if app.panes.len() <= 1 {
        if !app.panes.is_empty() {
            app.panes.clear();
        }
        app.focused_pane = 0;
        return;
    }

    let existing: Vec<SessionId> = app
        .manager
        .sessions()
        .into_iter()
        .map(|summary| summary.id)
        .collect();
    app.panes.retain(|id| existing.contains(id));
    let mut seen: Vec<SessionId> = Vec::new();
    app.panes.retain(|id| {
        if seen.contains(id) {
            false
        } else {
            seen.push(*id);
            true
        }
    });

    if app.panes.len() <= 1 {
        app.panes.clear();
        app.focused_pane = 0;
        return;
    }

    match app.manager.active_session() {
        Some(active) => match app.panes.iter().position(|id| *id == active) {
            Some(pos) => app.focused_pane = pos,
            None => {
                // A session outside the tiling became active (e.g. a fresh
                // connection): collapse the split and show it single.
                app.panes.clear();
                app.focused_pane = 0;
            }
        },
        None => {
            app.panes.clear();
            app.focused_pane = 0;
        }
    }

    if app.focused_pane >= app.panes.len() {
        app.focused_pane = app.panes.len().saturating_sub(1);
    }
}

/// Which credential-store entry holds the secret for a provider, or `None`
/// when the provider needs no secret.
#[must_use]
pub(crate) fn sync_secret_name(provider: adit_storage::SyncProvider) -> Option<&'static str> {
    use adit_storage::SyncProvider;
    match provider {
        SyncProvider::None => None,
        SyncProvider::Gist => Some("sync.gist.token"),
        SyncProvider::WebDav => Some("sync.webdav.password"),
        SyncProvider::S3 => Some("sync.s3.secret_key"),
        // For an OAuth provider the stored secret is the refresh token: the
        // one thing that must survive a restart, and the only thing worth
        // sealing. Access tokens are re-minted from it on demand.
        SyncProvider::GoogleDrive => Some("sync.gdrive.refresh_token"),
        SyncProvider::OneDrive => Some("sync.onedrive.refresh_token"),
        SyncProvider::Dropbox => Some("sync.dropbox.refresh_token"),
    }
}

/// The OAuth client id for a provider: the user's override if they set one,
/// otherwise whatever this build carries.
#[must_use]
pub(crate) fn sync_client_id(app: &AditApp, provider: adit_storage::SyncProvider) -> String {
    use adit_storage::SyncProvider;
    use adit_sync::backend::{client_id, OAuthProvider};
    match provider {
        SyncProvider::GoogleDrive => {
            client_id(OAuthProvider::GoogleDrive, &app.sync.google_client_id)
        }
        SyncProvider::OneDrive => client_id(OAuthProvider::OneDrive, &app.sync.onedrive_client_id),
        SyncProvider::Dropbox => client_id(OAuthProvider::Dropbox, &app.sync.dropbox_client_id),
        _ => String::new(),
    }
}

/// The OAuth client secret, for the one provider that demands one.
#[must_use]
pub(crate) fn sync_client_secret(app: &AditApp, provider: adit_storage::SyncProvider) -> String {
    use adit_storage::SyncProvider;
    use adit_sync::backend::{client_secret, OAuthProvider};
    match provider {
        SyncProvider::GoogleDrive => {
            client_secret(OAuthProvider::GoogleDrive, &app.sync.google_client_secret)
        }
        SyncProvider::OneDrive => client_secret(OAuthProvider::OneDrive, ""),
        SyncProvider::Dropbox => client_secret(OAuthProvider::Dropbox, ""),
        _ => String::new(),
    }
}

/// The authorisation to run for a provider, or `None` when this build has no
/// client id for it and the user supplied none either.
#[must_use]
pub(crate) fn sync_oauth_config(
    app: &AditApp,
    provider: adit_storage::SyncProvider,
) -> Option<adit_sync::backend::oauth::OAuthConfig> {
    use adit_storage::SyncProvider;
    use adit_sync::backend::{dropbox, gdrive, onedrive};
    let id = sync_client_id(app, provider);
    if id.trim().is_empty() {
        return None;
    }
    Some(match provider {
        SyncProvider::GoogleDrive => gdrive::oauth_config(id, sync_client_secret(app, provider)),
        SyncProvider::OneDrive => onedrive::oauth_config(id),
        SyncProvider::Dropbox => dropbox::oauth_config(id),
        _ => return None,
    })
}

/// The catalog as it stands right now: what a sync uploads and merges against.
#[must_use]
pub(crate) fn catalog_snapshot(app: &AditApp) -> adit_storage::ProfileCatalog {
    adit_storage::ProfileCatalog::new(app.groups.to_vec(), app.manager.profiles().to_vec())
}

/// This machine's name, for the "last written by" line. Cosmetic — a device
/// name is never an identity and never decides a merge.
#[must_use]
pub(crate) fn hostname_or_unknown() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| String::from("unknown"))
}

#[must_use]
pub(crate) fn rfc3339_now() -> String {
    adit_sync::rfc3339(std::time::SystemTime::now())
}

/// Build the configured backend, or `None` when the panel is not filled in
/// enough to try. Returning `None` rather than a half-configured backend keeps
/// "you have not finished setting this up" separate from "the server said no".
#[must_use]
pub(crate) fn build_sync_backend(app: &AditApp) -> Option<Box<dyn adit_sync::SyncBackend>> {
    use adit_storage::SyncProvider;
    use adit_sync::backend::{dropbox, gdrive, gist, onedrive, s3, webdav};

    let secret = sync_secret_name(app.sync.provider)
        .and_then(|name| app.credential_store.load_secret(name).ok().flatten())
        .unwrap_or_default();

    match app.sync.provider {
        SyncProvider::None => None,
        SyncProvider::Gist => (!secret.is_empty()).then(|| {
            Box::new(gist::GistBackend::new(gist::GistConfig {
                token: secret,
                gist_id: (!app.sync.gist_id.trim().is_empty())
                    .then(|| app.sync.gist_id.trim().to_owned()),
            })) as Box<dyn adit_sync::SyncBackend>
        }),
        SyncProvider::WebDav => (!app.sync.webdav_url.trim().is_empty() && !secret.is_empty())
            .then(|| {
                Box::new(webdav::WebDavBackend::new(webdav::WebDavConfig {
                    url: app.sync.webdav_url.trim().to_owned(),
                    username: app.sync.webdav_username.clone(),
                    password: secret,
                })) as Box<dyn adit_sync::SyncBackend>
            }),
        SyncProvider::S3 => (!app.sync.s3_endpoint.trim().is_empty()
            && !app.sync.s3_bucket.trim().is_empty()
            && !app.sync.s3_access_key.trim().is_empty()
            && !secret.is_empty())
        .then(|| {
            Box::new(s3::S3Backend::new(s3::S3Config {
                endpoint: app.sync.s3_endpoint.trim().to_owned(),
                region: app.sync.s3_region.clone(),
                bucket: app.sync.s3_bucket.trim().to_owned(),
                key: app.sync.s3_key.clone(),
                access_key: app.sync.s3_access_key.trim().to_owned(),
                secret_key: secret,
                path_style: app.sync.s3_path_style,
            })) as Box<dyn adit_sync::SyncBackend>
        }),
        // The OAuth three: the refresh token stands in for a password, and
        // its absence means "not connected yet" rather than "misconfigured".
        SyncProvider::GoogleDrive | SyncProvider::OneDrive | SyncProvider::Dropbox => {
            let id = sync_client_id(app, app.sync.provider);
            let client_secret = sync_client_secret(app, app.sync.provider);
            if id.trim().is_empty() || secret.is_empty() {
                return None;
            }
            Some(match app.sync.provider {
                SyncProvider::GoogleDrive => {
                    Box::new(gdrive::GoogleDriveBackend::new(id, client_secret, secret))
                        as Box<dyn adit_sync::SyncBackend>
                }
                SyncProvider::OneDrive => Box::new(onedrive::OneDriveBackend::new(id, secret))
                    as Box<dyn adit_sync::SyncBackend>,
                _ => Box::new(dropbox::DropboxBackend::new(id, secret))
                    as Box<dyn adit_sync::SyncBackend>,
            })
        }
    }
}

/// Snapshot the persistable preferences from live app state.
pub(crate) fn current_settings(app: &AditApp) -> AppSettings {
    AppSettings {
        language: app.language,
        sync: app.sync.clone(),
        dark_mode: app.dark_mode,
        theme_mode: Some(app.theme_mode),
        host_layout: app.host_layout,
        recent_hosts: app.recent_hosts.clone(),
        grid_order: app.grid_order.clone(),
        // BTreeSet iterates sorted, so the snapshot is order-stable.
        collapsed_groups: app.collapsed_groups.iter().cloned().collect(),
        window_width: app.window_width,
        window_height: app.window_height,
        auto_reconnect: app.manager.auto_reconnect(),
        sidebar_width: app.sidebar_width,
        sidebar_visible: app.sidebar_visible,
        font_family: app.font_family.clone(),
        font_size: app.font_size,
        color_scheme: app.color_scheme.clone(),
        highlight_rules: app.highlight_rules.clone(),
        log_dir: app.log_dir.clone(),
        log_name_pattern: app.log_name_pattern.clone(),
        auto_log_on_connect: app.auto_log_on_connect,
        log_plaintext: app.log_plaintext,
        copy_on_select: app.copy_on_select,
        right_click_paste: app.right_click_paste,
        confirm_multiline_paste: app.confirm_multiline_paste,
        connect_timeout_secs: app.connect_timeout_secs,
        scrollback_lines: app.scrollback_lines,
        snippets: app.snippets.clone(),
        auto_check_updates: app.auto_check_updates,
        command_window_open: app.command_window_open,
        command_send_immediately: app.command_send_immediately,
        auto_accept_host_keys: app.auto_accept_host_keys,
        rdp_clipboard: app.rdp_clipboard,
        keyring_migrated: app.keyring_migrated,
    }
}

/// Persist settings when they drift from what is on disk. Called every Tick so
/// any config change (theme, folded groups, window size) is debounced into at
/// most one write per frame and survives a restart.
pub(crate) fn persist_settings_if_changed(app: &mut AditApp) {
    let current = current_settings(app);
    if current == app.persisted_settings {
        return;
    }
    if let Err(error) = app.settings_store.save(&current) {
        app.last_error = Some(tf("保存设置失败: {}", &[&error]));
    }
    // Update the baseline regardless of outcome so a failing write does not
    // retry on every frame.
    app.persisted_settings = current;
}

/// Keep the SFTP path-bar edit buffers in sync with each pane's current
/// directory, clearing the per-pane selection when the directory changes.
pub(crate) fn sync_sftp_state(app: &mut AditApp) {
    let Some((remote, local)) = app
        .manager
        .sftp_browser()
        .map(|browser| (browser.cwd.clone(), browser.local_cwd.display().to_string()))
    else {
        // The SFTP session can close on its own (e.g. the channel drops), not just
        // via the close button — drop any open right-click menu so it can't reappear
        // over a freshly-reopened session referencing a stale entry.
        app.sftp_context_menu = None;
        return;
    };
    if remote != app.sftp_remote_cwd_seen {
        app.sftp_remote_cwd_seen = remote.clone();
        app.sftp_remote_path_edit = remote;
        app.sftp_remote_selected.clear();
    }
    if local != app.sftp_local_cwd_seen {
        app.sftp_local_cwd_seen = local.clone();
        app.sftp_local_path_edit = local;
        app.sftp_local_selected.clear();
    }
}
