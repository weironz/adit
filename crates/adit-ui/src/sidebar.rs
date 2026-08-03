use super::*;
use iced::widget::column;

/// The session tree itself, without the panel around it.
///
/// Extracted so the host manager's tree layout *is* this tree rather than a
/// second one that resembles it. Collapsing, drag-reordering, context menus and
/// filtering all live in here; a reimplementation would begin without them and
/// then drift.
pub(crate) fn session_tree(app: &AditApp) -> Element<'_, Message> {
    let mut sorted_profiles = app.manager.profiles().to_vec();
    sorted_profiles.sort_by(profile_sidebar_order);

    let filter = app.session_filter.trim().to_ascii_lowercase();
    let filter_active = !filter.is_empty();
    // No single "Hosts" root node: the top level of the tree holds ungrouped
    // sessions and folders, freely interleaved by their position.
    let mut profiles = column![].spacing(1).width(Fill);

    // Merge the top-level items — ungrouped sessions (keyed by their global
    // sort_order) and folders (keyed by their slot) — into one ordered list.
    let folders = sidebar_group_names(app, &sorted_profiles);
    enum TopEntry<'a> {
        Session(&'a ConnectionProfile),
        Folder(String),
    }
    let mut entries: Vec<(i32, TopEntry)> = Vec::new();
    for profile in sorted_profiles
        .iter()
        .filter(|profile| profile.group.trim().is_empty())
    {
        if filter_active && !profile_matches_filter(profile, &filter) {
            continue;
        }
        entries.push((profile.sort_order, TopEntry::Session(profile)));
    }
    for (index, group) in folders.iter().enumerate() {
        entries.push((
            (index as i32 + 1) * TOP_LEVEL_STEP,
            TopEntry::Folder(group.clone()),
        ));
    }
    entries.sort_by_key(|(key, _)| *key);

    // A real drag exposes drop zones at the very top and very bottom so a session
    // can be pulled out of any folder to the top level. Tree drags only: these
    // reorder the tree, and a drag that started on a grid card has no business
    // sprouting tree furniture in a panel it is not interacting with.
    if app.profile_drag_active && !app.drag_from_grid {
        profiles = profiles.push(top_level_drop_zone(
            app.profile_drop == Some(ProfileDrop::TopLevel),
        ));
    }

    for (_, entry) in entries {
        match entry {
            TopEntry::Session(profile) => {
                profiles = profiles.push(sidebar_profile_row(app, profile));
            }
            TopEntry::Folder(group) => {
                let group_matches = filter_active && group.to_ascii_lowercase().contains(&filter);
                let group_profiles = sorted_profiles
                    .iter()
                    .filter(|profile| profile.group == group)
                    .filter(|profile| {
                        !filter_active || group_matches || profile_matches_filter(profile, &filter)
                    })
                    .cloned()
                    .collect::<Vec<_>>();

                if filter_active && !group_matches && group_profiles.is_empty() {
                    continue;
                }

                let collapsed = app.collapsed_groups.contains(&group) && !filter_active;
                let group_count = sorted_profiles
                    .iter()
                    .filter(|candidate| candidate.group == group)
                    .count();
                let group_drop_target = app.group_drop_target.as_deref() == Some(group.as_str());
                let editing = app.editing_group.as_deref() == Some(group.as_str());
                // A folder-reorder insertion line above the header (dragging up) or
                // below the whole block (dragging down). Suppressed while renaming.
                let (line_before, line_after) = if editing {
                    (false, false)
                } else {
                    folder_reorder_lines(app, &folders, &group)
                };
                if line_before {
                    profiles = profiles.push(drop_line());
                }
                if editing {
                    // Rename in place: the header itself becomes an editable field.
                    profiles = profiles
                        .push(tree_group_edit_row(app.group_name_draft.clone(), collapsed));
                } else {
                    profiles = profiles.push(tree_group_row(
                        group.clone(),
                        collapsed,
                        group_count,
                        group_drop_target,
                    ));
                }
                if !collapsed {
                    for profile in &group_profiles {
                        profiles = profiles.push(sidebar_profile_row(app, profile));
                    }
                }
                if line_after {
                    profiles = profiles.push(drop_line());
                }
            }
        }
    }

    if app.profile_drag_active && !app.drag_from_grid {
        profiles = profiles.push(bottom_level_drop_zone(
            app.profile_drop == Some(ProfileDrop::BottomLevel),
        ));
    }

    profiles.into()
}

pub(crate) fn sidebar(app: &AditApp) -> Element<'_, Message> {
    let error = app
        .last_error
        .as_ref()
        .map(|message| {
            container(
                row![
                    text(message).size(12).color(danger()),
                    Space::new().width(Fill),
                    button("x")
                        .on_press(Message::ClearError)
                        .style(|_theme, status| close_button_style(status)),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .padding(8)
            .width(Fill)
            .style(|_theme| error_panel_style())
        })
        .map(Element::from);

    let mut content = column![
        container(
            row![
                text("Session Manager").size(13).color(primary_text()),
                Space::new().width(Fill),
                // Just the essentials on the right (SecureCRT-style): new session
                // and a button to hide the whole panel.
                sidebar_tool_button(
                    "▦",
                    "返回主机列表",
                    Message::ShowMainView(MainView::Hosts),
                ),
                sidebar_tool_button("⊕", "新建会话", Message::NewProfileDraft),
                sidebar_tool_button("«", "隐藏会话栏", Message::ToggleSidebar),
            ]
            .spacing(4)
            .align_y(Alignment::Center),
        )
        .height(Length::Fixed(30.0))
        .padding([2, 8])
        .style(|_theme| sidebar_header_style()),
        text_input("Filter by group/session name <Alt+I>", &app.session_filter)
            .id(session_filter_id())
            .on_input(Message::SessionFilterChanged)
            .padding([4, 6])
            .style(toolbar_input_style),
        scrollable(session_tree(app)).height(Fill),
    ]
    .spacing(0)
    .height(Fill)
    .width(Length::Fixed(app.sidebar_width));

    if let Some(error) = error {
        content = content.push(error);
    }

    // Track the cursor over the sidebar so a right-click can anchor its floating
    // context menu at the pointer, and (during a drag) so we can tell a click
    // from a drag. `on_move` gives sidebar-relative coordinates.
    mouse_area(
        container(content)
            .height(Fill)
            .style(|_theme| sidebar_style()),
    )
    .on_move(Message::SidebarCursorMoved)
    .into()
}

/// The draggable divider between the sidebar and the workspace. Pressing it
/// starts a resize drag; the drag itself is driven by the global cursor
/// subscription that is only active while `sidebar_dragging` is set.
pub(crate) fn sidebar_divider() -> Element<'static, Message> {
    mouse_area(
        container(Space::new().width(Length::Fixed(SIDEBAR_DIVIDER_WIDTH)).height(Fill))
            .height(Fill)
            .style(|_theme| container::Style {
                background: Some(Background::Color(border_color())),
                ..container::Style::default()
            }),
    )
    .on_press(Message::BeginSidebarDrag)
    .interaction(mouse::Interaction::ResizingHorizontally)
    .into()
}

/// Folders in display order: the user-arranged order first, then any folder seen
/// only on a profile. Empty ("ungrouped") is never a folder.
pub(crate) fn sidebar_group_names(app: &AditApp, profiles: &[ConnectionProfile]) -> Vec<String> {
    let mut groups: Vec<String> = app
        .groups
        .iter()
        .filter(|group| !group.trim().is_empty())
        .cloned()
        .collect();
    for group in groups_from_profiles(profiles) {
        if !groups.contains(&group) {
            groups.push(group);
        }
    }
    groups
}

pub(crate) fn tree_group_row(
    group: String,
    collapsed: bool,
    profile_count: usize,
    drop_target: bool,
) -> Element<'static, Message> {
    let arrow = if collapsed { "▸" } else { "▾" };
    let group_label = group.clone();
    let toggle_group = group.clone();
    let enter_group = group.clone();
    let hover_group = group.clone();
    let release_group = group.clone();
    let exit_group = group.clone();
    let context_group = group.clone();

    mouse_area(
        container(
            row![
                text(arrow).size(12).color(muted_text()),
                text(group_label).size(13).color(muted_text()),
                Space::new().width(Fill),
                text(profile_count.to_string()).size(10).color(muted_text()),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding([6, 8])
        .width(Fill)
        .style(move |_theme| group_row_style(drop_target)),
    )
    // Press arms a folder drag; a release with no real movement toggles collapse.
    .on_press(Message::GroupPressed(toggle_group))
    .on_right_press(Message::ShowGroupContextMenu(context_group))
    .on_enter(Message::ProfileDragOverGroup(enter_group))
    .on_move(move |_| Message::ProfileDragOverGroup(hover_group.clone()))
    .on_release(Message::ProfileDroppedOnGroup(release_group))
    .on_exit(Message::ProfileGroupHoverExited(exit_group))
    .interaction(mouse::Interaction::Pointer)
    .into()
}

/// The floating folder context menu (mirrors the session menu): rename, new
/// session, collapse/expand, and a destructive "delete folder" that removes the
/// folder and its session configs.
pub(crate) fn group_context_menu_card(group: String, collapsed: bool) -> Element<'static, Message> {
    let toggle_label = if collapsed { "展开" } else { "折叠" };
    container(
        column![
            profile_menu_item("重命名", Message::RenameGroupFromContext(group.clone()), false),
            profile_menu_item("新会话", Message::NewProfileInGroup(group.clone()), false),
            profile_menu_item(toggle_label, Message::ToggleProfileGroup(group.clone()), false),
            profile_menu_divider(),
            profile_menu_item("删除分组", Message::DeleteGroupFromContext(group), true),
        ]
        .spacing(1),
    )
    .padding(4)
    .width(Length::Fixed(PROFILE_MENU_WIDTH))
    .style(|_theme| profile_context_menu_style())
    .into()
}

pub(crate) fn group_context_overlay(app: &AditApp, group: String, collapsed: bool) -> Element<'_, Message> {
    floating_context_menu(
        app,
        group_context_menu_card(group, collapsed),
        Message::HideGroupContextMenu,
    )
}

/// A folder header in rename mode: the name is edited in place (no popup). Enter
/// or clicking away saves; Esc reverts. There are no confirm/cancel buttons.
pub(crate) fn tree_group_edit_row(draft: String, collapsed: bool) -> Element<'static, Message> {
    let arrow = if collapsed { "▸" } else { "▾" };
    container(
        row![
            text(arrow).size(11).color(muted_text()),
            text_input("分组名称", &draft)
                .id(rename_input_id())
                .on_input(Message::GroupNameDraftChanged)
                .on_submit(Message::SaveGroupRename)
                .padding([2, 6])
                .size(12)
                .style(text_input_style)
                .width(Fill),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding([4, 8])
    .width(Fill)
    .style(move |_theme| group_row_style(false))
    .into()
}

/// A session row in rename mode: the name line becomes an editable field in place
/// (no popup). Enter or clicking away saves; Esc reverts. No confirm/cancel buttons.
pub(crate) fn tree_profile_edit_row(profile: ConnectionProfile, draft: String) -> Element<'static, Message> {
    container(
        row![
            Space::new().width(Length::Fixed(4.0)),
            avatar(&profile.name),
            text_input("会话名称", &draft)
                .id(rename_input_id())
                .on_input(Message::ProfileNameDraftChanged)
                .on_submit(Message::SaveProfileRename)
                .padding([2, 6])
                .size(13)
                .style(text_input_style)
                .width(Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .height(Length::Fixed(PROFILE_ROW_HEIGHT))
    .width(Fill)
    .padding([2, 8])
    .style(move |_theme| tree_item_container_style(true, false, false))
    .into()
}

/// Render one session row for the sidebar. Shared by the ungrouped top-level
/// list and every group. During a drag the row stays put (SecureCRT-style) and
/// only an insertion line marks where the drop will land.
pub(crate) fn sidebar_profile_row(app: &AditApp, profile: &ConnectionProfile) -> Element<'static, Message> {
    // Rename in place: the row's name becomes an editable field (no popup).
    if app.editing_profile == Some(profile.id) {
        return tree_profile_edit_row(profile.clone(), app.profile_name_draft.clone());
    }
    // The dragged row stays in place (SecureCRT-style); only an insertion line at
    // the target shows where it will land. It reads as selected while dragging.
    let selected = Some(profile.id) == app.selected_profile;
    let hovered = Some(profile.id) == app.hovered_profile;
    let row = tree_profile_row(profile.clone(), selected, hovered, false);

    // An insertion line above or below this row when it is the drop target.
    // Never for a grid drag: the drop data is shared state, and the profile it
    // names has a row here too — without this gate the tree draws furniture for
    // a drag that has nothing to do with it.
    let (before, after) = if !app.profile_drag_active || app.drag_from_grid {
        (false, false)
    } else {
        match &app.profile_drop {
            Some(ProfileDrop::Beside {
                profile_id,
                position,
            }) if *profile_id == profile.id => match position {
                ProfileDropPosition::Before => (true, false),
                ProfileDropPosition::After => (false, true),
            },
            _ => (false, false),
        }
    };
    if !before && !after {
        return row;
    }
    let mut col = column![].spacing(0).width(Fill);
    if before {
        col = col.push(drop_line());
    }
    col = col.push(row);
    if after {
        col = col.push(drop_line());
    }
    col.into()
}

/// A thin accent bar marking where a dragged session will be inserted.
pub(crate) fn drop_line() -> Element<'static, Message> {
    container(Space::new().width(Fill).height(Length::Fixed(2.0)))
        .width(Fill)
        .padding([1, 6])
        .style(|_theme| container::Style {
            background: Some(Background::Color(accent())),
            border: border(1.0, 0.0, transparent()),
            ..container::Style::default()
        })
        .into()
}

/// A drop zone at the very top of the tree (shown only during a drag) so a
/// session can be pulled out of any group to the very top level.
pub(crate) fn top_level_drop_zone(targeted: bool) -> Element<'static, Message> {
    edge_drop_zone(targeted, "↥ 拖到此处 · 移到最顶（顶层）", Message::ProfileDragOverTop)
}

/// A drop zone at the very bottom of the tree so a session can be placed below
/// every folder at the top level.
pub(crate) fn bottom_level_drop_zone(targeted: bool) -> Element<'static, Message> {
    edge_drop_zone(
        targeted,
        "↧ 拖到此处 · 移到最底（顶层）",
        Message::ProfileDragOverBottom,
    )
}

pub(crate) fn edge_drop_zone(
    targeted: bool,
    label_text: &'static str,
    on_enter: Message,
) -> Element<'static, Message> {
    let indicator: Element<'static, Message> = if targeted {
        drop_line()
    } else {
        Space::new().width(Fill).height(Length::Fixed(2.0)).into()
    };
    let label = if targeted { accent() } else { muted_text() };
    mouse_area(
        container(
            column![indicator, text(label_text).size(10).color(label)].spacing(2),
        )
        .width(Fill)
        .padding([2, 8])
        .style(move |_theme| container::Style {
            background: Some(Background::Color(if targeted {
                accent_soft()
            } else {
                surface_alt()
            })),
            border: border(RADIUS_SM, 1.0, if targeted { accent() } else { border_color() }),
            ..container::Style::default()
        }),
    )
    // on_enter arms the drop; the global release (CancelProfileDrag) commits it.
    .on_enter(on_enter)
    .into()
}

pub(crate) fn tree_profile_row(
    profile: ConnectionProfile,
    selected: bool,
    hovered: bool,
    dragging: bool,
) -> Element<'static, Message> {
    let profile_id = profile.id;
    // Windows/winit renders `Grab` as the no-entry cursor, which reads as
    // "forbidden" on a plain hover. Use the normal arrow when idle and a 4-way
    // move cursor (a native Windows cursor) while actually dragging to reorder.
    let interaction = if dragging {
        mouse::Interaction::Move
    } else {
        mouse::Interaction::Idle
    };

    mouse_area(
        container(
            row![
                Space::new().width(Length::Fixed(4.0)),
                avatar(&profile.name),
                // Name only — the user@host subtitle was too busy in a long list.
                text(profile.name.clone()).size(13).color(primary_text()),
                Space::new().width(Fill),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .height(Length::Fixed(PROFILE_ROW_HEIGHT))
        .width(Fill)
        .padding([2, 8])
        .style(move |_theme| tree_item_container_style(selected, hovered, dragging)),
    )
    .on_press(Message::ProfilePressed(profile_id))
    .on_release(Message::ProfileDropped(profile_id))
    .on_double_click(Message::ProfileDoubleClicked(profile_id))
    .on_right_press(Message::ShowProfileContextMenu(profile_id))
    .on_enter(Message::ProfileHovered(profile_id))
    .on_move(move |point| Message::ProfileDragOver(profile_id, profile_drop_position(point)))
    .on_exit(Message::ProfileHoverExited(profile_id))
    .interaction(interaction)
    .into()
}

pub(crate) fn profile_drop_position(point: Point) -> ProfileDropPosition {
    if point.y < PROFILE_ROW_HEIGHT / 2.0 {
        ProfileDropPosition::Before
    } else {
        ProfileDropPosition::After
    }
}

/// A Termius-style round host avatar: the host's initials on a per-host color.
pub(crate) fn avatar(name: &str) -> Element<'static, Message> {
    let color = avatar_color(name);
    container(text(avatar_initials(name)).size(9).color(Color::WHITE))
        .center_x(Length::Fixed(18.0))
        .center_y(Length::Fixed(18.0))
        .style(move |_theme| avatar_style(color))
        .into()
}

pub(crate) fn avatar_initials(name: &str) -> String {
    let mut initials = String::new();
    for token in name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .take(2)
    {
        if let Some(first) = token.chars().next() {
            initials.push(first);
        }
    }
    if initials.is_empty() {
        initials.push('?');
    }
    initials.to_uppercase()
}

pub(crate) fn avatar_color(name: &str) -> Color {
    let hash = name
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(u32::from(b)));
    match hash % 6 {
        0 => Color::from_rgb8(15, 158, 140),  // teal
        1 => Color::from_rgb8(44, 123, 214),  // blue
        2 => Color::from_rgb8(124, 116, 221), // purple
        3 => Color::from_rgb8(216, 90, 48),   // coral
        4 => Color::from_rgb8(196, 78, 122),  // pink
        _ => Color::from_rgb8(78, 140, 46),   // green
    }
}

pub(crate) fn avatar_style(color: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(color)),
        text_color: Some(Color::WHITE),
        border: border(16.0, 0.0, transparent()),
        ..container::Style::default()
    }
}

pub(crate) const PROFILE_MENU_WIDTH: f32 = 168.0;
pub(crate) const PROFILE_MENU_HEIGHT: f32 = 162.0;

/// The context-menu card (used inside the floating overlay).
pub(crate) fn profile_context_menu(profile_id: ProfileId) -> Element<'static, Message> {
    container(
        column![
            profile_menu_item("连接", Message::ConnectProfileFromContext(profile_id), false),
            profile_menu_item("重命名", Message::RenameProfileFromContext(profile_id), false),
            profile_menu_item("编辑", Message::EditProfileFromContext(profile_id), false),
            profile_menu_item("克隆", Message::CloneProfileFromContext(profile_id), false),
            profile_menu_divider(),
            profile_menu_item("删除", Message::DeleteProfileFromContext(profile_id), true),
        ]
        .spacing(1),
    )
    .padding(4)
    .width(Length::Fixed(PROFILE_MENU_WIDTH))
    .style(|_theme| profile_context_menu_style())
    .into()
}

/// A floating context menu anchored at the cursor, over a transparent scrim that
/// dismisses it on any outside click.
pub(crate) fn profile_context_overlay(app: &AditApp, profile_id: ProfileId) -> Element<'_, Message> {
    floating_context_menu(
        app,
        profile_context_menu(profile_id),
        Message::HideProfileContextMenu,
    )
}

pub(crate) fn tab_context_overlay(app: &AditApp, session_id: SessionId) -> Element<'_, Message> {
    // Connected sessions can be disconnected; already-disconnected ones offer a
    // reconnect instead.
    let connected = app.manager.session_summary(session_id).is_some_and(|s| {
        matches!(
            s.status,
            SessionStatus::Connected | SessionStatus::Connecting
        )
    });
    floating_context_menu(
        app,
        tab_context_menu(session_id, connected),
        Message::HideTabContextMenu,
    )
}

/// The session-tab right-click menu card (rename / (dis)connect / clone / close).
pub(crate) fn tab_context_menu(session_id: SessionId, connected: bool) -> Element<'static, Message> {
    let connection_item = if connected {
        profile_menu_item("断开连接", Message::DisconnectSession(session_id), false)
    } else {
        profile_menu_item("重新连接", Message::ReconnectSession(session_id), false)
    };
    container(
        column![
            profile_menu_item("重命名", Message::RenameSessionPrompt(session_id), false),
            connection_item,
            profile_menu_item("克隆会话", Message::CloneSessionFromTab(session_id), false),
            profile_menu_divider(),
            profile_menu_item("关闭", Message::CloseSession(session_id), true),
        ]
        .spacing(1),
    )
    .padding(4)
    .width(Length::Fixed(PROFILE_MENU_WIDTH))
    .style(|_theme| profile_context_menu_style())
    .into()
}

pub(crate) fn terminal_context_overlay(app: &AditApp) -> Element<'_, Message> {
    floating_context_menu(
        app,
        terminal_context_menu(),
        Message::HideTerminalContextMenu,
    )
}

/// Place a context-menu `card` at the last-tracked cursor position (clamped to
/// the window) over a scrim that dismisses it on any outside click.
pub(crate) fn floating_context_menu<'a>(
    app: &AditApp,
    card: Element<'a, Message>,
    hide: Message,
) -> Element<'a, Message> {
    let x = app
        .context_menu_pos
        .x
        .min((app.window_width - PROFILE_MENU_WIDTH - 6.0).max(0.0))
        .max(0.0);
    let y = app
        .context_menu_pos
        .y
        .min((app.window_height - PROFILE_MENU_HEIGHT - 6.0).max(0.0))
        .max(0.0);

    let positioned = column![
        Space::new().height(Length::Fixed(y)),
        row![Space::new().width(Length::Fixed(x)), card],
    ]
    .width(Fill)
    .height(Fill);

    stack![
        mouse_area(Space::new().width(Fill).height(Fill))
            .on_press(hide.clone())
            .on_right_press(hide),
        positioned,
    ]
    .into()
}

pub(crate) fn profile_menu_item(label: &'static str, message: Message, danger: bool) -> Element<'static, Message> {
    button(text(label).size(12))
        .width(Fill)
        .padding([6, 10])
        .style(move |_theme, status| profile_menu_item_style(status, danger))
        .on_press(message)
        .into()
}

pub(crate) fn profile_menu_divider() -> Element<'static, Message> {
    container(Space::new().height(Length::Fixed(1.0)))
        .width(Fill)
        .style(|_theme| container::Style {
            background: Some(Background::Color(border_color())),
            ..container::Style::default()
        })
        .into()
}


pub(crate) fn profile_matches_filter(profile: &ConnectionProfile, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }

    profile.group.to_ascii_lowercase().contains(filter)
        || profile.name.to_ascii_lowercase().contains(filter)
        || profile.host.to_ascii_lowercase().contains(filter)
        || profile.username.to_ascii_lowercase().contains(filter)
        || profile.endpoint().to_ascii_lowercase().contains(filter)
}

pub(crate) fn profile_sidebar_order(
    left: &ConnectionProfile,
    right: &ConnectionProfile,
) -> std::cmp::Ordering {
    left.group
        .cmp(&right.group)
        .then_with(|| left.sort_order.cmp(&right.sort_order))
        .then_with(|| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
        })
        .then_with(|| left.host.cmp(&right.host))
}

pub(crate) fn sidebar_tool_button(
    glyph: &'static str,
    tip: &'static str,
    message: Message,
) -> Element<'static, Message> {
    let control = button(text(glyph).size(14))
        .width(Length::Fixed(28.0))
        .height(Length::Fixed(26.0))
        .padding(0)
        .style(|_theme, status| sidebar_tool_button_style(status))
        .on_press(message);

    tooltip(
        control,
        container(text(tip).size(11).color(primary_text()))
            .padding([3, 8])
            .style(|_theme| tooltip_style()),
        tooltip::Position::Bottom,
    )
    .gap(4)
    .into()
}

pub(crate) fn tooltip_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.0, border_color()),
        shadow: subtle_shadow(),
        ..container::Style::default()
    }
}
