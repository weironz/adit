use super::*;
use iced::widget::column;

#[derive(Default)]
pub(crate) struct Missions {
    items: Vec<Mission>,
    active: Option<u64>,
    next_id: u64,
    pub(super) picker: bool,
    name_draft: Option<String>,
}

struct Mission {
    id: u64,
    name: String,
    members: Vec<SessionId>,
    active: Option<SessionId>,
    focused: bool,
    mode: TileMode,
}

pub(crate) fn change_main_view(app: &mut AditApp, view: MainView) {
    release_rdp_modifiers(app);
    app.active_menu = None;
    app.tab_context_menu = None;
    app.profile_context_menu = None;
    app.group_context_menu = None;
    app.terminal_context_menu = false;
    app.move_to_group_menu = false;
    app.rdp_quality_menu_open = false;
    app.terminal_selecting = false;
    if app.main_view == MainView::Missions {
        remember_mission(app);
    }
    let previous_panes = std::mem::take(&mut app.panes);
    app.focused_pane = 0;
    app.main_view = view;
    app.missions.picker = false;
    app.missions.name_draft = None;
    app.terminal_focused = view != MainView::Hosts;
    if view == MainView::Missions {
        if app.missions.items.is_empty() {
            create_mission(app, previous_panes);
        } else {
            restore_mission(app);
        }
    }
    sync_terminal_size(app);
}

fn remember_mission(app: &mut AditApp) {
    let active = app.manager.active_session();
    if let Some(task) = app
        .missions
        .items
        .iter_mut()
        .find(|m| Some(m.id) == app.missions.active)
    {
        if active.is_some_and(|id| task.members.contains(&id)) {
            task.active = active;
        }
        task.mode = app.tile_mode;
    }
}

pub(crate) fn create_mission(app: &mut AditApp, mut members: Vec<SessionId>) {
    finish_mission_name(app, true);
    remember_mission(app);
    if members.is_empty() {
        members = app
            .manager
            .sessions()
            .iter()
            .filter(|s| app.selected_tabs.contains(&s.id))
            .map(|s| s.id)
            .take(MAX_PANES)
            .collect();
    }
    if members.is_empty() {
        members.extend(app.manager.active_session());
    }
    members.truncate(MAX_PANES);
    app.missions.next_id += 1;
    let id = app.missions.next_id;
    app.missions.items.push(Mission {
        id,
        name: tf("任务 {}", &[&id]),
        active: members.first().copied(),
        members,
        focused: false,
        mode: TileMode::Grid,
    });
    app.missions.active = Some(id);
    app.main_view = MainView::Missions;
    restore_mission(app);
}

pub(crate) fn pick_mission(app: &mut AditApp, id: u64) {
    if !app.missions.items.iter().any(|m| m.id == id) {
        return;
    }
    finish_mission_name(app, true);
    remember_mission(app);
    release_rdp_modifiers(app);
    app.missions.active = Some(id);
    app.missions.picker = false;
    app.main_view = MainView::Missions;
    restore_mission(app);
}

pub(crate) fn restore_mission(app: &mut AditApp) {
    let Some(task) = app
        .missions
        .items
        .iter()
        .find(|m| Some(m.id) == app.missions.active)
    else {
        app.panes.clear();
        app.terminal_focused = false;
        return;
    };
    let active = task
        .active
        .filter(|id| task.members.contains(id))
        .or_else(|| task.members.first().copied());
    app.panes = if task.focused || task.members.len() < 2 || app.fullscreen {
        Vec::new()
    } else {
        task.members.clone()
    };
    app.tile_mode = task.mode;
    app.focused_pane = active
        .and_then(|id| app.panes.iter().position(|p| *p == id))
        .unwrap_or(0);
    if let Some(id) = active {
        if app.manager.active_session() != Some(id) {
            release_rdp_modifiers(app);
            let _ = app.manager.activate(id);
            app.terminal_selection = None;
            app.terminal_scroll_offset = 0;
        }
    }
    app.terminal_focused = active.is_some() && !app.missions.picker;
    sync_terminal_size(app);
}

pub(crate) fn add_mission_session(app: &mut AditApp, id: SessionId) {
    if app.manager.session_summary(id).is_none() {
        return;
    }
    if let Some(task) = app
        .missions
        .items
        .iter_mut()
        .find(|m| Some(m.id) == app.missions.active)
    {
        if !task.members.contains(&id) {
            if task.members.len() >= MAX_PANES {
                app.notice = tf("每个任务最多 {} 个窗格", &[&MAX_PANES]);
                return;
            }
            task.members.push(id);
        }
        task.active = Some(id);
        task.focused = false;
    }
    app.missions.picker = false;
    restore_mission(app);
}

pub(crate) fn split_mission(app: &mut AditApp) {
    if app.main_view != MainView::Missions {
        change_main_view(app, MainView::Missions);
    }
    let members = app
        .missions
        .items
        .iter()
        .find(|m| Some(m.id) == app.missions.active)
        .map(|m| m.members.clone())
        .unwrap_or_default();
    let candidate = app
        .manager
        .sessions()
        .iter()
        .find(|s| !members.contains(&s.id))
        .map(|s| s.id);
    if let Some(id) = candidate {
        add_mission_session(app, id);
    } else {
        app.notice = String::from(t("没有更多会话可分屏（先在资源库打开另一个会话）"));
    }
}

pub(crate) fn tile_mission(app: &mut AditApp, members: Vec<SessionId>, mode: TileMode) {
    if app.main_view != MainView::Missions {
        change_main_view(app, MainView::Missions);
    }
    if let Some(task) = app
        .missions
        .items
        .iter_mut()
        .find(|m| Some(m.id) == app.missions.active)
    {
        task.members = members;
        task.active = app
            .manager
            .active_session()
            .filter(|id| task.members.contains(id))
            .or_else(|| task.members.first().copied());
        task.mode = mode;
        task.focused = false;
    }
    restore_mission(app);
}

pub(crate) fn remove_mission_pane(app: &mut AditApp, index: usize) {
    let Some(id) = app.panes.get(index).copied() else {
        return;
    };
    release_rdp_modifiers(app);
    if let Some(task) = app
        .missions
        .items
        .iter_mut()
        .find(|m| Some(m.id) == app.missions.active)
    {
        task.members.retain(|s| *s != id);
        if task.active == Some(id) {
            task.active = task.members.first().copied();
        }
    }
    restore_mission(app);
}

pub(crate) fn focus_mission(app: &mut AditApp, index: Option<usize>) {
    if let Some(index) = index {
        focus_pane(app, index);
    }
    remember_mission(app);
    release_rdp_modifiers(app);
    if let Some(task) = app
        .missions
        .items
        .iter_mut()
        .find(|m| Some(m.id) == app.missions.active)
    {
        task.focused = index.is_some();
    }
    restore_mission(app);
}

pub(crate) fn reconcile_missions(app: &mut AditApp) {
    let existing: HashSet<_> = app.manager.sessions().iter().map(|s| s.id).collect();
    let mut changed = false;
    for task in &mut app.missions.items {
        let before = task.members.len();
        task.members.retain(|id| existing.contains(id));
        changed |= before != task.members.len();
        if task.active.is_some_and(|id| !existing.contains(&id)) {
            task.active = task.members.first().copied();
        }
    }
    if app.main_view == MainView::Missions
        && (changed || (!mission_is_empty(app) && !mission_has_active_member(app)))
    {
        restore_mission(app);
    } else if app.main_view != MainView::Missions && !app.panes.is_empty() {
        app.panes.clear();
        app.focused_pane = 0;
        sync_terminal_size(app);
    }
}

pub(crate) fn mission_is_empty(app: &AditApp) -> bool {
    app.missions
        .items
        .iter()
        .find(|m| Some(m.id) == app.missions.active)
        .is_none_or(|m| m.members.is_empty())
}

pub(crate) fn mission_has_active_member(app: &AditApp) -> bool {
    app.manager.active_session().is_some_and(|id| {
        app.missions
            .items
            .iter()
            .find(|m| Some(m.id) == app.missions.active)
            .is_some_and(|m| m.members.contains(&id))
    })
}

pub(crate) fn mission_title(app: &AditApp) -> String {
    app.missions
        .items
        .iter()
        .find(|m| Some(m.id) == app.missions.active)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| String::from(t("任务分屏")))
}

pub(crate) fn mission_sidebar(app: &AditApp) -> Element<'_, Message> {
    let mut tasks = column![].spacing(6);
    for task in &app.missions.items {
        let selected = app.missions.active == Some(task.id);
        tasks = tasks.push(
            button(
                column![
                    text(task.name.clone()).size(TEXT_BODY),
                    text(tf("{} 个窗格", &[&task.members.len()]))
                        .size(TEXT_META)
                        .color(muted_text()),
                ]
                .spacing(4),
            )
            .padding(10)
            .width(Fill)
            .style(move |_, status| menu_button_style(selected, status))
            .on_press(Message::PickMission(task.id)),
        );
    }
    let mut content = column![
        row![
            text(t("任务分屏")).size(TEXT_BODY).width(Fill),
            sidebar_tool_button("+", 20.0, "新建任务", Message::NewMission),
            sidebar_tool_button("«", 14.0, "隐藏会话栏", Message::ToggleSidebar),
        ]
        .height(TAB_BAR_HEIGHT)
        .align_y(Alignment::Center),
        scrollable(tasks).height(Fill),
    ]
    .spacing(SPACE_SM);
    if let Some(task) = app
        .missions
        .items
        .iter()
        .find(|m| Some(m.id) == app.missions.active)
    {
        if let Some(draft) = &app.missions.name_draft {
            content = content
                .push(
                    text_input(t("任务名称"), draft)
                        .id(mission_name_id())
                        .on_input(Message::MissionNameChanged)
                        .on_submit(Message::FinishMissionName(true))
                        .padding(8)
                        .style(text_input_style),
                )
                .push(
                    row![
                        button(text(t("保存"))).on_press(Message::FinishMissionName(true)),
                        button(text(t("取消"))).on_press(Message::FinishMissionName(false)),
                    ]
                    .spacing(8),
                );
        } else {
            content = content.push(
                button(text(t("重命名任务")).size(TEXT_META))
                    .on_press(Message::BeginMissionName)
                    .padding(8)
                    .width(Fill)
                    .style(|_, s| secondary_button_style(s)),
            );
        }
        for id in &task.members {
            if let Some(summary) = app.manager.session_summary(*id) {
                content = content.push(
                    button(text(summary.title).size(TEXT_META))
                        .width(Fill)
                        .padding(6)
                        .style(move |_, status| {
                            menu_button_style(app.manager.active_session() == Some(*id), status)
                        })
                        .on_press(Message::ActivateMissionSession(*id)),
                );
            }
        }
        content = content.push(
            button(text(t("添加已打开会话")))
                .padding(8)
                .width(Fill)
                .on_press(Message::MissionPickerToggled)
                .style(|_, s| secondary_button_style(s)),
        );
        if app.missions.picker {
            let mut choices = column![].spacing(3);
            for summary in app.manager.sessions() {
                if !task.members.contains(&summary.id) {
                    choices = choices.push(
                        button(text(summary.title).size(TEXT_META))
                            .width(Fill)
                            .on_press(Message::AddMissionSession(summary.id)),
                    );
                }
            }
            content = content.push(scrollable(choices).height(160));
        }
        content = content.push(
            button(text(t("移除任务（保留连接）")).size(TEXT_META))
                .on_press(Message::RemoveMission)
                .style(|_, s| secondary_button_style(s)),
        );
    }
    container(content)
        .padding(12)
        .width(Length::Fixed(app.sidebar_width))
        .height(Fill)
        .style(|_| sidebar_style())
        .into()
}

pub(crate) fn rename_mission(app: &mut AditApp, name: String) {
    app.terminal_focused = false;
    app.missions.name_draft = Some(name);
}

pub(crate) fn mission_name_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("mission-name")
}

pub(crate) fn begin_mission_name(app: &mut AditApp) -> Task<Message> {
    release_rdp_modifiers(app);
    app.terminal_focused = false;
    app.missions.name_draft = app
        .missions
        .items
        .iter()
        .find(|m| Some(m.id) == app.missions.active)
        .map(|m| m.name.clone());
    iced::advanced::widget::operate(iced::advanced::widget::operation::focusable::focus(
        mission_name_id(),
    ))
}

pub(crate) fn mission_name_editing(app: &AditApp) -> bool {
    app.missions.name_draft.is_some()
}

pub(crate) fn finish_mission_name(app: &mut AditApp, commit: bool) {
    if let Some(draft) = app
        .missions
        .name_draft
        .take()
        .filter(|s| commit && !s.trim().is_empty())
    {
        if let Some(task) = app
            .missions
            .items
            .iter_mut()
            .find(|m| Some(m.id) == app.missions.active)
        {
            task.name = draft.trim().to_owned();
        }
    }
    app.terminal_focused = app.main_view == MainView::Missions && !mission_is_empty(app);
}

pub(crate) fn remove_mission(app: &mut AditApp) {
    release_rdp_modifiers(app);
    app.missions.name_draft = None;
    app.missions.picker = false;
    app.missions
        .items
        .retain(|m| Some(m.id) != app.missions.active);
    app.missions.active = app.missions.items.first().map(|m| m.id);
    restore_mission(app);
}

pub(crate) fn toggle_mission_picker(app: &mut AditApp) {
    release_rdp_modifiers(app);
    app.missions.picker = !app.missions.picker;
    app.terminal_focused = !app.missions.picker && mission_has_active_member(app);
}

pub(crate) fn activate_mission_session(app: &mut AditApp, id: SessionId) {
    finish_mission_name(app, true);
    if let Some(task) = app
        .missions
        .items
        .iter_mut()
        .find(|m| Some(m.id) == app.missions.active)
    {
        if !task.members.contains(&id) {
            return;
        }
        task.active = Some(id);
    }
    restore_mission(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (AditApp, Vec<SessionId>) {
        let profiles: Vec<_> = (0..12)
            .map(|n| {
                ConnectionProfile::with_group(
                    "fixture",
                    format!("fixture-{n}"),
                    "127.0.0.1",
                    22,
                    "fixture",
                )
            })
            .collect();
        let mut app = AditApp {
            manager: SessionManager::with_profiles(profiles),
            ..AditApp::default()
        };
        let profile_ids: Vec<_> = app.manager.profiles().iter().map(|p| p.id).collect();
        let sessions = profile_ids
            .iter()
            .map(|id| app.manager.open_mock_session(*id).unwrap())
            .collect();
        (app, sessions)
    }

    fn session_ids(app: &AditApp) -> HashSet<SessionId> {
        app.manager.sessions().iter().map(|s| s.id).collect()
    }

    #[test]
    fn twelve_sessions_survive_views_tasks_focus_hide_and_removal() {
        let (mut app, ids) = fixture();
        let original = session_ids(&app);
        change_main_view(&mut app, MainView::Missions);
        let first = app.missions.active.unwrap();
        add_mission_session(&mut app, ids[0]);
        add_mission_session(&mut app, ids[1]);
        let split = app.panes.clone();
        focus_mission(&mut app, Some(1));
        let focused = app.manager.active_session();
        assert!(app.panes.is_empty());
        focus_mission(&mut app, None);
        assert_eq!(app.panes, split);
        assert_eq!(app.manager.active_session(), focused);
        change_main_view(&mut app, MainView::Hosts);
        assert!(!app.terminal_focused);
        assert!(app.panes.is_empty());
        change_main_view(&mut app, MainView::Terminal);
        assert!(app.panes.is_empty());
        change_main_view(&mut app, MainView::Missions);
        assert_eq!(app.panes, split);
        create_mission(&mut app, vec![ids[3], ids[4]]);
        let second = app.missions.active.unwrap();
        assert_ne!(first, second);
        assert_eq!(app.panes, vec![ids[3], ids[4]]);
        pick_mission(&mut app, first);
        assert_eq!(app.panes, split);
        remove_mission_pane(&mut app, 0);
        assert_eq!(session_ids(&app), original);
        app.sidebar_visible = false;
        remove_mission(&mut app);
        assert_eq!(app.missions.active, Some(second));
        assert_eq!(session_ids(&app), original);
        assert_eq!(original.len(), 12);
    }

    #[test]
    fn closing_session_prunes_every_task_without_closing_other_members() {
        let (mut app, ids) = fixture();
        create_mission(&mut app, vec![ids[0], ids[1]]);
        let first = app.missions.active.unwrap();
        create_mission(&mut app, vec![ids[0], ids[2]]);
        app.manager.close(ids[0]);
        reconcile_missions(&mut app);
        assert!(app
            .missions
            .items
            .iter()
            .all(|m| !m.members.contains(&ids[0])));
        pick_mission(&mut app, first);
        assert_eq!(app.manager.active_session(), Some(ids[1]));
        assert_eq!(app.manager.sessions().len(), 11);
    }

    #[test]
    fn duplicate_members_and_capacity_do_not_mutate_connections() {
        let (mut app, ids) = fixture();
        create_mission(&mut app, vec![ids[0]]);
        for id in &ids {
            add_mission_session(&mut app, *id);
        }
        let task = &app.missions.items[0];
        assert_eq!(task.members.len(), MAX_PANES);
        assert_eq!(task.members.iter().collect::<HashSet<_>>().len(), MAX_PANES);
        assert_eq!(app.manager.sessions().len(), 12);
    }

    #[test]
    fn fullscreen_and_local_tools_share_the_rendered_terminal_origin() {
        let (mut app, ids) = fixture();
        create_mission(&mut app, vec![ids[0], ids[1], ids[2], ids[3]]);
        for sidebar in [false, true] {
            app.sidebar_visible = sidebar;
            for scale in [1.0, 1.5, 2.0] {
                app.display_scale = scale;
                let layout = pane_layout(&app);
                assert_eq!(layout.origin_x, sidebar_offset(&app));
                assert_eq!(layout.origin_y, MENU_BAR_HEIGHT + TAB_BAR_HEIGHT);
                let point = layout.pane_body_origin(app.focused_pane);
                assert_eq!(window_to_pane_local(&app, point), Point::ORIGIN);
                app.search_open = true;
                assert_eq!(
                    pane_layout(&app).origin_y,
                    MENU_BAR_HEIGHT + TAB_BAR_HEIGHT + TOOLBAR_HEIGHT
                );
                app.search_open = false;
            }
        }
        app.fullscreen = true;
        let layout = pane_layout(&app);
        assert_eq!(layout.cols, 1);
        assert_eq!(layout.origin_x, 0.0);
        assert_eq!(layout.origin_y, TAB_BAR_HEIGHT);
        assert_eq!(layout.header, 0.0);
        assert_eq!(app.manager.sessions().len(), 12);
    }

    #[test]
    fn local_task_controls_block_remote_input_and_cancel_restores_focus() {
        let (mut app, ids) = fixture();
        create_mission(&mut app, vec![ids[0], ids[1]]);
        assert!(remote_workspace_accepts_input(&app));
        let original = app.missions.items[0].name.clone();
        let _ = update_loop::update(&mut app, Message::BeginMissionName);
        assert!(!remote_workspace_accepts_input(&app));
        rename_mission(&mut app, "edited".into());
        finish_mission_name(&mut app, false);
        assert_eq!(app.missions.items[0].name, original);
        assert!(remote_workspace_accepts_input(&app));
        toggle_mission_picker(&mut app);
        assert!(!remote_workspace_accepts_input(&app));
        toggle_mission_picker(&mut app);
        app.tab_context_menu = Some(ids[0]);
        assert!(!remote_workspace_accepts_input(&app));
        app.tab_context_menu = None;
        change_main_view(&mut app, MainView::Hosts);
        assert!(!remote_workspace_accepts_input(&app));
    }

    fn key(ch: char, modifiers: keyboard::Modifiers) -> Message {
        let key = Key::Character(ch.to_string().into());
        Message::KeyboardInput(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        })
    }

    #[test]
    fn filtered_shift_selection_cannot_close_a_hidden_session() {
        let (mut app, ids) = fixture();
        app.manager.rename_session(ids[0], "shown-A");
        app.manager.rename_session(ids[1], "hidden-B");
        app.manager.rename_session(ids[2], "shown-C");
        let _ = update_loop::update(&mut app, Message::OpenSessionFilterChanged("shown".into()));
        let _ = update_loop::update(&mut app, Message::TabClicked(ids[0]));
        app.modifiers = keyboard::Modifiers::SHIFT;
        let _ = update_loop::update(&mut app, Message::TabClicked(ids[2]));
        assert_eq!(app.selected_tabs, HashSet::from([ids[0], ids[2]]));
        let _ = update_loop::update(&mut app, Message::CloseSelectedTabs);
        assert!(app.manager.session_summary(ids[1]).is_some());
        assert_eq!(app.manager.sessions().len(), 10);
        app.selected_tabs.insert(ids[1]);
        let _ = update_loop::update(&mut app, Message::OpenSessionFilterChanged("shown".into()));
        assert!(app.selected_tabs.is_empty());
    }

    #[test]
    fn empty_task_never_owns_input_for_a_background_connection() {
        let (mut app, ids) = fixture();
        create_mission(&mut app, vec![ids[0]]);
        app.manager.close(ids[0]);
        reconcile_missions(&mut app);
        toggle_mission_picker(&mut app);
        toggle_mission_picker(&mut app);
        assert!(!app.terminal_focused);
        assert!(!mission_has_active_member(&app));
        app.terminal_focused = true;
        assert!(!remote_workspace_accepts_input(&app));
        assert_eq!(app.manager.sessions().len(), 11);
    }

    #[test]
    fn fullscreen_return_reprojects_the_task_after_window_resize() {
        let (mut app, ids) = fixture();
        create_mission(&mut app, ids[..4].to_vec());
        focus_mission(&mut app, Some(2));
        app.fullscreen = true;
        focus_mission(&mut app, None);
        assert!(app.panes.is_empty());
        app.fullscreen = false;
        let _ = update_loop::update(
            &mut app,
            Message::WindowResized {
                width: 1360.0,
                height: 860.0,
                window: window::Id::unique(),
            },
        );
        assert_eq!(app.panes, ids[..4]);
        assert_eq!(app.manager.active_session(), Some(ids[2]));
    }

    #[test]
    fn global_navigation_dismisses_menus_and_blocked_keys_cannot_copy_profiles() {
        let (mut app, _) = fixture();
        change_main_view(&mut app, MainView::Terminal);
        let profiles: Vec<_> = app.manager.profiles().iter().map(|p| p.id).collect();
        app.selected_profile = Some(profiles[0]);
        app.profile_clipboard = vec![profiles[1]];
        app.active_menu = Some(MenuKind::File);
        let _ = update_loop::update(&mut app, key('c', keyboard::Modifiers::CTRL));
        assert_eq!(app.profile_clipboard, vec![profiles[1]]);
        change_main_view(&mut app, MainView::Missions);
        assert!(app.active_menu.is_none());
        assert!(remote_workspace_accepts_input(&app));
    }

    #[test]
    fn local_tool_bars_resize_the_terminal_in_both_directions() {
        let (mut app, _) = fixture();
        change_main_view(&mut app, MainView::Terminal);
        let original = app.terminal_size;
        let _ = update_loop::update(
            &mut app,
            key('f', keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT),
        );
        assert!(app.search_open);
        assert!(app.terminal_size.rows < original.rows);
        let _ = update_loop::update(&mut app, Message::CloseSearch);
        assert_eq!(app.terminal_size, original);
        let _ = update_loop::update(&mut app, Message::ToggleCommandWindow);
        assert!(app.terminal_size.rows < original.rows);
        let _ = update_loop::update(&mut app, Message::ToggleCommandWindow);
        assert_eq!(app.terminal_size, original);
    }
}
