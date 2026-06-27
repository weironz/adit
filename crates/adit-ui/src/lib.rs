use adit_domain::{AuthMethod, ConnectionProfile, ProfileId, SessionId, SessionStatus};
use adit_session::{ProfileMove, ProfileSortKey, SessionManager, SessionSummary};
use adit_storage::ProfileStore;
use adit_terminal::{Color as TermColor, TerminalLine, TerminalSize, TerminalSnapshot, Viewport};
use iced::font::Weight;
use iced::keyboard::{self, key::Named, Key};
use iced::widget::{
    button, column, container, mouse_area, row, scrollable, text, text_input, Space,
};
use iced::{
    clipboard, event, mouse, window, Alignment, Background, Border, Color, Element, Fill, Font,
    Length, Point, Subscription, Task, Theme,
};
use std::time::Duration;

pub struct AditApp {
    manager: SessionManager,
    profile_store: ProfileStore,
    selected_profile: Option<ProfileId>,
    active_menu: Option<MenuKind>,
    profile_folder: String,
    profile_name: String,
    profile_host: String,
    profile_port: String,
    profile_username: String,
    profile_auth_method: AuthMethod,
    profile_identity_file: String,
    password: String,
    session_filter: String,
    terminal_input: String,
    terminal_focused: bool,
    terminal_size: TerminalSize,
    terminal_pointer: Option<TerminalPoint>,
    terminal_selection: Option<TerminalSelection>,
    terminal_selecting: bool,
    terminal_context_menu: bool,
    terminal_scroll_offset: usize,
    window_width: f32,
    window_height: f32,
    last_error: Option<String>,
    notice: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    File,
    Session,
    Edit,
    View,
    Transfer,
    Script,
    Tools,
    Help,
}

#[derive(Debug, Clone, Copy)]
pub enum MenuCommand {
    NewProfile,
    SaveProfile,
    DeleteProfile,
    Connect,
    Disconnect,
    OpenMockTab,
    CloseActiveTab,
    ClearTerminal,
    ResizeDefault,
    ResizeWide,
    Sftp,
    Logging,
    About,
}

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    ToggleMenu(MenuKind),
    RunMenu(MenuCommand),
    SelectProfile(ProfileId),
    ProfileFolderChanged(String),
    ProfileNameChanged(String),
    ProfileHostChanged(String),
    ProfilePortChanged(String),
    ProfileUsernameChanged(String),
    ProfileAuthMethodChanged(AuthMethod),
    ProfileIdentityFileChanged(String),
    SessionFilterChanged(String),
    NewProfileDraft,
    SaveProfile,
    DeleteSelectedProfile,
    MoveSelectedProfile(ProfileMove),
    SortProfiles(ProfileSortKey),
    PasswordChanged(String),
    TerminalInputChanged(String),
    KeyboardInput(keyboard::Event),
    WindowResized { width: f32, height: f32 },
    FocusTerminal,
    TerminalPointerMoved(Point),
    TerminalScrolled(mouse::ScrollDelta),
    BeginTerminalSelection,
    EndTerminalSelection,
    ShowTerminalContextMenu,
    HideTerminalContextMenu,
    CopyTerminalSelection,
    PasteIntoTerminal,
    ClipboardPasted(Option<String>),
    TerminalJumpToBottom,
    OpenSelectedProfile,
    ConnectSelectedProfile,
    ActivateSession(SessionId),
    CloseSession(SessionId),
    DisconnectActive,
    SendTerminalInput,
    ClearActiveTerminal,
    ClearError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalPoint {
    row: usize,
    col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalSelection {
    start: TerminalPoint,
    end: TerminalPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalScrollAction {
    Lines(i32),
    Top,
    Bottom,
}

const TERMINAL_CHAR_WIDTH: f32 = 7.8;
const TERMINAL_ROW_HEIGHT: f32 = 17.0;
const SIDEBAR_WIDTH: f32 = 348.0;
const MENU_BAR_HEIGHT: f32 = 24.0;
const MENU_PANEL_HEIGHT: f32 = 34.0;
const TOOLBAR_HEIGHT: f32 = 30.0;
const WORKSPACE_PADDING_X: f32 = 0.0;
const WORKSPACE_PADDING_TOP: f32 = 0.0;
const TAB_BAR_HEIGHT: f32 = 30.0;
const TERMINAL_PANEL_PADDING: f32 = 6.0;
const TERMINAL_HEADER_AND_GAP: f32 = 0.0;
const TERMINAL_CONTEXT_MENU_AND_GAP: f32 = 34.0;

impl Default for AditApp {
    fn default() -> Self {
        let profile_store = ProfileStore::default();
        let load_result = profile_store.load_profiles();
        let (manager, load_notice, load_error) = match load_result {
            Ok(profiles) if !profiles.is_empty() => {
                let count = profiles.len();
                (
                    SessionManager::with_profiles(profiles),
                    format!(
                        "已加载 {count} 个会话配置: {}",
                        profile_store.path().display()
                    ),
                    None,
                )
            }
            Ok(_) => (
                SessionManager::with_demo_profiles(),
                format!(
                    "使用演示会话配置，保存后写入 {}",
                    profile_store.path().display()
                ),
                None,
            ),
            Err(error) => (
                SessionManager::with_demo_profiles(),
                format!(
                    "使用演示会话配置，保存后写入 {}",
                    profile_store.path().display()
                ),
                Some(format!("读取会话配置失败: {error}")),
            ),
        };
        let selected_profile = manager.profiles().first().map(|profile| profile.id);
        let window_width = 1360.0;
        let window_height = 860.0;
        let mut app = Self {
            manager,
            profile_store,
            selected_profile,
            active_menu: None,
            profile_folder: String::new(),
            profile_name: String::new(),
            profile_host: String::new(),
            profile_port: String::from("22"),
            profile_username: String::new(),
            profile_auth_method: AuthMethod::Auto,
            profile_identity_file: String::new(),
            password: String::new(),
            session_filter: String::new(),
            terminal_input: String::new(),
            terminal_focused: false,
            terminal_size: estimated_terminal_size(window_width, window_height, false),
            terminal_pointer: None,
            terminal_selection: None,
            terminal_selecting: false,
            terminal_context_menu: false,
            terminal_scroll_offset: 0,
            window_width,
            window_height,
            last_error: load_error,
            notice: load_notice,
        };
        load_selected_profile(&mut app);
        app
    }
}

pub fn run() -> iced::Result {
    iced::application(AditApp::default, update, view)
        .title(app_title)
        .theme(app_theme)
        .subscription(subscription)
        .window_size((1360.0, 860.0))
        .centered()
        .run()
}

fn app_title(app: &AditApp) -> String {
    format!("Adit - {}", app.manager.status_line())
}

fn app_theme(_app: &AditApp) -> Theme {
    Theme::Dark
}

fn subscription(_app: &AditApp) -> Subscription<Message> {
    Subscription::batch([
        iced::time::every(Duration::from_millis(100)).map(|_| Message::Tick),
        event::listen_with(runtime_event),
    ])
}

fn runtime_event(
    event: event::Event,
    status: event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        event::Event::Keyboard(event) if status == event::Status::Ignored => {
            Some(Message::KeyboardInput(event))
        }
        event::Event::Window(window::Event::Opened { size, .. })
        | event::Event::Window(window::Event::Resized(size)) => Some(Message::WindowResized {
            width: size.width,
            height: size.height,
        }),
        _ => None,
    }
}

fn update(app: &mut AditApp, message: Message) -> Task<Message> {
    match message {
        Message::Tick => {
            app.manager.poll_events();
            clamp_terminal_scroll(app);
        }
        Message::ToggleMenu(menu) => {
            app.active_menu = if app.active_menu == Some(menu) {
                None
            } else {
                Some(menu)
            };
            sync_terminal_size(app);
        }
        Message::RunMenu(command) => {
            run_menu_command(app, command);
            app.active_menu = None;
            sync_terminal_size(app);
        }
        Message::SelectProfile(profile_id) => {
            app.terminal_focused = false;
            app.selected_profile = Some(profile_id);
            load_selected_profile(app);
            app.last_error = None;
        }
        Message::ProfileFolderChanged(value) => {
            app.terminal_focused = false;
            app.profile_folder = value;
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
        Message::ProfileIdentityFileChanged(value) => {
            app.terminal_focused = false;
            app.profile_identity_file = value;
        }
        Message::SessionFilterChanged(value) => {
            app.terminal_focused = false;
            app.session_filter = value;
        }
        Message::NewProfileDraft => {
            new_profile_draft(app);
        }
        Message::SaveProfile => {
            save_profile(app);
        }
        Message::DeleteSelectedProfile => {
            delete_selected_profile(app);
        }
        Message::MoveSelectedProfile(direction) => {
            move_selected_profile(app, direction);
        }
        Message::SortProfiles(key) => {
            sort_profiles(app, key);
        }
        Message::PasswordChanged(password) => {
            app.terminal_focused = false;
            app.password = password;
        }
        Message::TerminalInputChanged(input) => {
            app.terminal_focused = false;
            app.terminal_input = input;
        }
        Message::KeyboardInput(event) => {
            if !app.terminal_focused {
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
        Message::WindowResized { width, height } => {
            app.window_width = width;
            app.window_height = height;
            sync_terminal_size(app);
        }
        Message::FocusTerminal => {
            if !app.terminal_focused {
                app.notice = String::from("终端已聚焦，键盘输入会发送到当前会话");
            }
            app.terminal_focused = true;
            app.terminal_context_menu = false;
        }
        Message::TerminalPointerMoved(point) => {
            let terminal_point = terminal_point_from_cursor(app, point);
            app.terminal_pointer = Some(terminal_point);

            if app.terminal_selecting {
                if let Some(selection) = &mut app.terminal_selection {
                    selection.end = terminal_point;
                }
            }
        }
        Message::TerminalScrolled(delta) => {
            app.terminal_focused = true;
            if let Some(lines) = scroll_delta_to_rows(delta) {
                apply_terminal_scroll(app, TerminalScrollAction::Lines(lines));
            }
        }
        Message::BeginTerminalSelection => {
            app.terminal_focused = true;
            app.terminal_context_menu = false;
            let point = app
                .terminal_pointer
                .unwrap_or(TerminalPoint { row: 0, col: 0 });
            app.terminal_selection = Some(TerminalSelection {
                start: point,
                end: point,
            });
            app.terminal_selecting = true;
        }
        Message::EndTerminalSelection => {
            app.terminal_selecting = false;
            if app
                .terminal_selection
                .is_some_and(|selection| selection.start == selection.end)
            {
                app.terminal_selection = None;
            }
        }
        Message::ShowTerminalContextMenu => {
            app.terminal_focused = true;
            app.terminal_selecting = false;
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
                let bytes = normalize_paste(&contents);
                if !bytes.is_empty() {
                    send_terminal_bytes(app, bytes);
                    app.notice = String::from("已粘贴到当前终端");
                }
            }
        }
        Message::TerminalJumpToBottom => {
            apply_terminal_scroll(app, TerminalScrollAction::Bottom);
        }
        Message::OpenSelectedProfile => {
            open_selected_mock_tab(app);
        }
        Message::ConnectSelectedProfile => {
            connect_selected_profile(app);
        }
        Message::ActivateSession(session_id) => {
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
        Message::CloseSession(session_id) => {
            app.manager.close(session_id);
            app.terminal_scroll_offset = 0;
            app.terminal_selection = None;
            app.terminal_context_menu = false;
            app.notice = String::from("标签已关闭");
        }
        Message::DisconnectActive => {
            disconnect_active(app);
        }
        Message::SendTerminalInput => {
            send_terminal_input(app);
        }
        Message::ClearActiveTerminal => {
            clear_active_terminal(app);
        }
        Message::ClearError => {
            app.last_error = None;
        }
    }

    Task::none()
}

fn run_menu_command(app: &mut AditApp, command: MenuCommand) {
    match command {
        MenuCommand::NewProfile => new_profile_draft(app),
        MenuCommand::SaveProfile => save_profile(app),
        MenuCommand::DeleteProfile => delete_selected_profile(app),
        MenuCommand::Connect => connect_selected_profile(app),
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
            app.notice = String::from("SFTP 面板将在后续里程碑接入");
        }
        MenuCommand::Logging => {
            app.notice = String::from("会话日志配置将在持久化模块后接入");
        }
        MenuCommand::About => {
            app.notice = String::from("Adit native prototype: iced + russh + Rust terminal core");
        }
    }
}

fn load_selected_profile(app: &mut AditApp) {
    let profile = app
        .selected_profile
        .and_then(|profile_id| app.manager.profile(profile_id).cloned());

    if let Some(profile) = profile {
        app.profile_folder = profile.folder;
        app.profile_name = profile.name;
        app.profile_host = profile.host;
        app.profile_port = profile.port.to_string();
        app.profile_username = profile.username;
        app.profile_auth_method = profile.auth_method;
        app.profile_identity_file = profile.identity_file;
    }
}

fn new_profile_draft(app: &mut AditApp) {
    let name = next_profile_name(app);
    match app.manager.create_profile(
        "Default",
        name,
        "127.0.0.1",
        22,
        "root",
        AuthMethod::Auto,
        "",
    ) {
        Ok(profile_id) => {
            app.selected_profile = Some(profile_id);
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

fn next_profile_name(app: &AditApp) -> String {
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

fn save_profile(app: &mut AditApp) {
    let _ = save_profile_from_form(app, true);
}

fn save_profile_from_form(app: &mut AditApp, show_notice: bool) -> Option<ProfileId> {
    let Some(port) = parse_port(&app.profile_port) else {
        app.last_error = Some(String::from("端口必须是 1-65535 的数字"));
        return None;
    };

    let result = if let Some(profile_id) = app.selected_profile {
        app.manager.update_profile(
            profile_id,
            app.profile_folder.clone(),
            app.profile_name.clone(),
            app.profile_host.clone(),
            port,
            app.profile_username.clone(),
            app.profile_auth_method,
            app.profile_identity_file.clone(),
        )
    } else {
        match app.manager.create_profile(
            app.profile_folder.clone(),
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
            load_selected_profile(app);
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

fn delete_selected_profile(app: &mut AditApp) {
    let Some(profile_id) = app.selected_profile else {
        app.last_error = Some(String::from("请选择要删除的会话配置"));
        return;
    };

    match app.manager.delete_profile(profile_id) {
        Ok(()) => {
            app.selected_profile = app.manager.profiles().first().map(|profile| profile.id);
            app.last_error = None;
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

fn move_selected_profile(app: &mut AditApp, direction: ProfileMove) {
    let Some(profile_id) = app.selected_profile else {
        app.last_error = Some(String::from("请选择要排序的会话配置"));
        return;
    };

    match app.manager.move_profile(profile_id, direction) {
        Ok(()) => {
            if persist_profiles(app) {
                app.last_error = None;
                app.notice = match direction {
                    ProfileMove::Up => String::from("会话已上移"),
                    ProfileMove::Down => String::from("会话已下移"),
                };
            }
        }
        Err(error) => app.last_error = Some(error.to_string()),
    }
}

fn sort_profiles(app: &mut AditApp, key: ProfileSortKey) {
    app.manager.sort_profiles(key);
    if persist_profiles(app) {
        app.last_error = None;
        app.notice = match key {
            ProfileSortKey::Name => String::from("会话已按名称排序"),
            ProfileSortKey::Host => String::from("会话已按主机排序"),
        };
    }
}

fn persist_profiles(app: &mut AditApp) -> bool {
    match app.profile_store.save_profiles(app.manager.profiles()) {
        Ok(()) => true,
        Err(error) => {
            app.last_error = Some(format!("保存会话配置失败: {error}"));
            false
        }
    }
}

fn parse_port(value: &str) -> Option<u16> {
    value.trim().parse::<u16>().ok().filter(|port| *port > 0)
}

fn open_selected_mock_tab(app: &mut AditApp) {
    if let Some(profile_id) = save_profile_from_form(app, false) {
        match app.manager.open_mock_session(profile_id) {
            Ok(_) => {
                app.terminal_focused = true;
                app.terminal_scroll_offset = 0;
                app.terminal_selection = None;
                app.terminal_context_menu = false;
                sync_terminal_size(app);
                app.last_error = None;
                app.notice = String::from("已打开演示标签");
            }
            Err(error) => app.last_error = Some(error.to_string()),
        }
    }
}

fn connect_selected_profile(app: &mut AditApp) {
    let Some(profile_id) = save_profile_from_form(app, false) else {
        return;
    };
    let endpoint = app
        .manager
        .profile(profile_id)
        .map(|profile| profile.endpoint())
        .unwrap_or_else(|| String::from("unknown"));

    match app
        .manager
        .open_live_ssh_session(profile_id, app.password.clone())
    {
        Ok(_) => {
            app.terminal_focused = true;
            app.terminal_scroll_offset = 0;
            app.terminal_selection = None;
            app.terminal_context_menu = false;
            sync_terminal_size(app);
            app.last_error = None;
            app.notice = format!("SSH 会话已开始连接: {endpoint}");
        }
        Err(error) => {
            app.last_error = Some(error.to_string());
        }
    }
}

fn disconnect_active(app: &mut AditApp) {
    if let Some(session_id) = app.manager.active_session() {
        match app.manager.disconnect(session_id) {
            Ok(()) => {
                app.last_error = None;
                app.notice = String::from("已请求断开当前会话");
            }
            Err(error) => app.last_error = Some(error.to_string()),
        }
    } else {
        app.last_error = Some(String::from("没有活动会话"));
    }
}

fn send_terminal_input(app: &mut AditApp) {
    if app.terminal_input.trim().is_empty() {
        return;
    }

    let mut input = app.terminal_input.clone();
    input.push('\r');

    match app.manager.send_input_to_active(input) {
        Ok(()) => {
            app.terminal_input.clear();
            app.terminal_scroll_offset = 0;
            app.terminal_selection = None;
            app.last_error = None;
        }
        Err(error) => {
            app.last_error = Some(error.to_string());
        }
    }
}

fn send_terminal_bytes(app: &mut AditApp, bytes: Vec<u8>) {
    if app.manager.active_session().is_none() {
        return;
    }

    app.terminal_scroll_offset = 0;
    app.terminal_selection = None;

    if let Err(error) = app.manager.send_input_bytes_to_active(bytes) {
        app.last_error = Some(error.to_string());
    }
}

fn is_terminal_copy_shortcut(event: &keyboard::Event) -> bool {
    terminal_shortcut(event, 'c')
}

fn is_terminal_paste_shortcut(event: &keyboard::Event) -> bool {
    terminal_shortcut(event, 'v')
}

fn terminal_shortcut(event: &keyboard::Event, key: char) -> bool {
    let keyboard::Event::KeyPressed {
        key: logical_key,
        physical_key,
        modifiers,
        ..
    } = event
    else {
        return false;
    };

    modifiers.control()
        && modifiers.shift()
        && logical_key
            .to_latin(*physical_key)
            .is_some_and(|pressed| pressed.eq_ignore_ascii_case(&key))
}

fn terminal_scroll_shortcut(
    event: &keyboard::Event,
    visible_rows: u16,
) -> Option<TerminalScrollAction> {
    let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
        return None;
    };

    if !modifiers.shift() {
        return None;
    }

    let Key::Named(named) = key else {
        return None;
    };

    let page = i32::from(visible_rows.saturating_sub(1).max(1));
    match *named {
        Named::PageUp => Some(TerminalScrollAction::Lines(page)),
        Named::PageDown => Some(TerminalScrollAction::Lines(-page)),
        Named::Home if modifiers.control() => Some(TerminalScrollAction::Top),
        Named::End if modifiers.control() => Some(TerminalScrollAction::Bottom),
        _ => None,
    }
}

fn normalize_paste(contents: &str) -> Vec<u8> {
    contents
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r")
        .into_bytes()
}

fn selected_terminal_text(app: &AditApp) -> String {
    let snapshot = active_terminal_snapshot(app);

    if let Some(selection) = app.terminal_selection {
        let selected = selection_to_text(&snapshot, selection);
        if !selected.is_empty() {
            return selected;
        }
    }

    snapshot_to_text(&snapshot)
}

fn active_terminal_snapshot(app: &AditApp) -> TerminalSnapshot {
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

fn terminal_view_rows(app: &AditApp) -> usize {
    usize::from(app.terminal_size.rows).max(1)
}

fn max_scroll_offset_for(snapshot: &TerminalSnapshot, rows: usize) -> usize {
    snapshot.total_rows.saturating_sub(rows.max(1))
}

fn max_terminal_scroll_offset(app: &AditApp) -> usize {
    let rows = terminal_view_rows(app);
    let snapshot = app.manager.active_snapshot(Viewport::tail(rows));
    max_scroll_offset_for(&snapshot, rows)
}

fn clamp_terminal_scroll(app: &mut AditApp) {
    let max_offset = max_terminal_scroll_offset(app);
    app.terminal_scroll_offset = app.terminal_scroll_offset.min(max_offset);
}

fn apply_terminal_scroll(app: &mut AditApp, action: TerminalScrollAction) {
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
        app.terminal_selection = None;
        app.notice = if next == 0 {
            String::from("终端已回到底部")
        } else {
            format!("终端历史: 距底部 {next} 行")
        };
    }
}

fn scroll_delta_to_rows(delta: mouse::ScrollDelta) -> Option<i32> {
    let raw = match delta {
        mouse::ScrollDelta::Lines { y, .. } => y * 3.0,
        mouse::ScrollDelta::Pixels { y, .. } => y / TERMINAL_ROW_HEIGHT,
    };

    if raw.abs() < f32::EPSILON {
        return None;
    }

    let rounded = raw.round();
    if rounded == 0.0 {
        Some(raw.signum() as i32)
    } else {
        Some(rounded as i32)
    }
}

fn snapshot_to_text(snapshot: &TerminalSnapshot) -> String {
    snapshot
        .lines
        .iter()
        .map(line_to_text)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

fn line_to_text(line: &TerminalLine) -> String {
    raw_line_text(line).trim_end().to_string()
}

fn raw_line_text(line: &TerminalLine) -> String {
    line.cells
        .iter()
        .map(|cell| cell.text.as_str())
        .collect::<String>()
}

fn selection_to_text(snapshot: &TerminalSnapshot, selection: TerminalSelection) -> String {
    let Some((start, end)) = normalized_selection(selection) else {
        return String::new();
    };

    let mut lines = Vec::new();
    for row_index in start.row..=end.row {
        let Some(line) = snapshot.lines.get(row_index) else {
            continue;
        };

        let text = raw_line_text(line);
        let chars = text.chars().collect::<Vec<_>>();
        let start_col = if row_index == start.row { start.col } else { 0 };
        let end_col = if row_index == end.row {
            end.col
        } else {
            chars.len()
        };

        if start_col >= end_col || start_col >= chars.len() {
            lines.push(String::new());
            continue;
        }

        let end_col = end_col.min(chars.len());
        lines.push(chars[start_col..end_col].iter().collect::<String>());
    }

    lines.join("\n").trim_end().to_string()
}

fn normalized_selection(selection: TerminalSelection) -> Option<(TerminalPoint, TerminalPoint)> {
    let (start, end) =
        if (selection.start.row, selection.start.col) <= (selection.end.row, selection.end.col) {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };

    if start == end {
        None
    } else {
        Some((start, end))
    }
}

fn selection_range_for_row(selection: TerminalSelection, row: usize) -> Option<(usize, usize)> {
    let (start, end) = normalized_selection(selection)?;
    if row < start.row || row > end.row {
        return None;
    }

    let start_col = if row == start.row { start.col } else { 0 };
    let end_col = if row == end.row { end.col } else { usize::MAX };

    (start_col < end_col).then_some((start_col, end_col))
}

fn terminal_point_from_cursor(app: &AditApp, point: Point) -> TerminalPoint {
    let menu_height = if app.active_menu.is_some() {
        MENU_PANEL_HEIGHT
    } else {
        0.0
    };
    let context_menu_height = if app.terminal_context_menu {
        TERMINAL_CONTEXT_MENU_AND_GAP
    } else {
        0.0
    };
    let origin_x = SIDEBAR_WIDTH + WORKSPACE_PADDING_X + TERMINAL_PANEL_PADDING;
    let origin_y = MENU_BAR_HEIGHT
        + menu_height
        + TOOLBAR_HEIGHT
        + WORKSPACE_PADDING_TOP
        + TAB_BAR_HEIGHT
        + TERMINAL_PANEL_PADDING
        + TERMINAL_HEADER_AND_GAP
        + context_menu_height;

    let col = ((point.x - origin_x) / TERMINAL_CHAR_WIDTH)
        .floor()
        .max(0.0) as usize;
    let row = ((point.y - origin_y) / TERMINAL_ROW_HEIGHT)
        .floor()
        .max(0.0) as usize;

    TerminalPoint {
        row: row.min(usize::from(app.terminal_size.rows.saturating_sub(1))),
        col: col.min(usize::from(app.terminal_size.cols)),
    }
}

fn encode_keyboard_event(event: keyboard::Event) -> Option<Vec<u8>> {
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

fn control_byte(key: &Key, physical_key: keyboard::key::Physical) -> Option<u8> {
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

fn named_key_sequence(named: Named, modifiers: keyboard::Modifiers) -> Option<&'static str> {
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

fn clear_active_terminal(app: &mut AditApp) {
    match app.manager.clear_active_terminal() {
        Ok(()) => {
            app.terminal_scroll_offset = 0;
            app.terminal_selection = None;
            app.terminal_context_menu = false;
            app.last_error = None;
            app.notice = String::from("当前终端已清屏");
        }
        Err(error) => app.last_error = Some(error.to_string()),
    }
}

fn resize_active(app: &mut AditApp, cols: u16, rows: u16) {
    match app.manager.resize_active(cols, rows) {
        Ok(()) => {
            app.terminal_size = TerminalSize::new(cols, rows);
            app.last_error = None;
            app.notice = format!("当前终端尺寸已设置为 {cols}x{rows}");
        }
        Err(error) => app.last_error = Some(error.to_string()),
    }
}

fn estimated_terminal_size(width: f32, height: f32, menu_open: bool) -> TerminalSize {
    const WORKSPACE_HORIZONTAL_PADDING: f32 = 0.0;
    const TERMINAL_HORIZONTAL_PADDING: f32 = TERMINAL_PANEL_PADDING * 2.0;
    const STATUS_BAR_HEIGHT: f32 = 22.0;
    const WORKSPACE_VERTICAL_PADDING: f32 = 0.0;
    const COMMAND_BAR_HEIGHT: f32 = 0.0;
    const TERMINAL_VERTICAL_CHROME: f32 = TERMINAL_PANEL_PADDING * 2.0;

    let menu_height = if menu_open { MENU_PANEL_HEIGHT } else { 0.0 };
    let available_width =
        width - SIDEBAR_WIDTH - WORKSPACE_HORIZONTAL_PADDING - TERMINAL_HORIZONTAL_PADDING;
    let available_height = height
        - MENU_BAR_HEIGHT
        - menu_height
        - TOOLBAR_HEIGHT
        - STATUS_BAR_HEIGHT
        - WORKSPACE_VERTICAL_PADDING
        - TAB_BAR_HEIGHT
        - COMMAND_BAR_HEIGHT
        - TERMINAL_VERTICAL_CHROME;

    let cols = (available_width / TERMINAL_CHAR_WIDTH)
        .floor()
        .clamp(40.0, 220.0) as u16;
    let rows = (available_height / TERMINAL_ROW_HEIGHT)
        .floor()
        .clamp(12.0, 80.0) as u16;

    TerminalSize::new(cols, rows)
}

fn sync_terminal_size(app: &mut AditApp) {
    let target = estimated_terminal_size(
        app.window_width,
        app.window_height,
        app.active_menu.is_some(),
    );

    if target == app.terminal_size {
        return;
    }

    app.terminal_size = target;
    if app.manager.active_session().is_some() {
        if let Err(error) = app.manager.resize_active(target.cols, target.rows) {
            app.last_error = Some(error.to_string());
        }
    }
}

fn view(app: &AditApp) -> Element<'_, Message> {
    let mut layout = column![menu_bar(app)];

    if let Some(menu) = app.active_menu {
        layout = layout.push(menu_panel(menu));
    }

    let layout = layout
        .push(toolbar(app))
        .push(row![sidebar(app), workspace(app)].height(Fill).width(Fill))
        .push(status_bar(app))
        .height(Fill)
        .width(Fill);

    container(layout)
        .style(|_theme| app_background_style())
        .height(Fill)
        .width(Fill)
        .into()
}

fn menu_bar(app: &AditApp) -> Element<'_, Message> {
    container(
        row![
            text("▣").size(14).color(accent()),
            menu_button(app, MenuKind::File, "File"),
            menu_button(app, MenuKind::Session, "Session"),
            menu_button(app, MenuKind::Edit, "Edit"),
            menu_button(app, MenuKind::View, "View"),
            menu_button(app, MenuKind::Transfer, "Transfer"),
            menu_button(app, MenuKind::Script, "Script"),
            menu_button(app, MenuKind::Tools, "Tools"),
            menu_button(app, MenuKind::Help, "Help"),
            Space::new().width(Fill),
            text(app.manager.status_line()).size(11).color(muted_text()),
        ]
        .spacing(2)
        .align_y(Alignment::Center),
    )
    .padding([2, 7])
    .height(24)
    .width(Fill)
    .style(|_theme| top_bar_style())
    .into()
}

fn menu_button<'a>(app: &AditApp, kind: MenuKind, label: &'a str) -> Element<'a, Message> {
    let active = app.active_menu == Some(kind);

    button(text(label).size(13))
        .padding([6, 10])
        .style(move |_theme, status| menu_button_style(active, status))
        .on_press(Message::ToggleMenu(kind))
        .into()
}

fn menu_panel(menu: MenuKind) -> Element<'static, Message> {
    let commands = match menu {
        MenuKind::File => row![
            command_button("新建会话", MenuCommand::NewProfile),
            command_button("保存会话", MenuCommand::SaveProfile),
            command_button("删除会话", MenuCommand::DeleteProfile),
            command_button("关闭标签", MenuCommand::CloseActiveTab),
        ],
        MenuKind::Session => row![
            command_button("连接", MenuCommand::Connect),
            command_button("断开", MenuCommand::Disconnect),
            command_button("打开演示标签", MenuCommand::OpenMockTab),
            command_button("关闭标签", MenuCommand::CloseActiveTab),
        ],
        MenuKind::Edit => row![command_button("清屏", MenuCommand::ClearTerminal)],
        MenuKind::View => row![
            command_button("终端 96x28", MenuCommand::ResizeDefault),
            command_button("终端 120x36", MenuCommand::ResizeWide),
        ],
        MenuKind::Transfer => row![command_button("SFTP", MenuCommand::Sftp)],
        MenuKind::Script => row![command_button("日志/脚本", MenuCommand::Logging)],
        MenuKind::Tools => row![
            command_button("清屏", MenuCommand::ClearTerminal),
            command_button("日志", MenuCommand::Logging),
        ],
        MenuKind::Help => row![command_button("关于", MenuCommand::About)],
    };

    container(commands.spacing(4).align_y(Alignment::Center))
        .padding([4, 8])
        .height(34)
        .width(Fill)
        .style(|_theme| menu_panel_style())
        .into()
}

fn command_button(label: &'static str, command: MenuCommand) -> Element<'static, Message> {
    button(label)
        .padding([4, 8])
        .style(|_theme, status| menu_command_button_style(status))
        .on_press(Message::RunMenu(command))
        .into()
}

fn toolbar(app: &AditApp) -> Element<'_, Message> {
    container(
        row![
            tool_button("↯", Message::ConnectSelectedProfile),
            tool_button("■", Message::DisconnectActive),
            tool_button("+", Message::NewProfileDraft),
            tool_button("□", Message::SaveProfile),
            tool_button("×", Message::DeleteSelectedProfile),
            tool_separator(),
            tool_button("↺", Message::OpenSelectedProfile),
            tool_button("⌫", Message::ClearActiveTerminal),
            tool_button("⇅", Message::RunMenu(MenuCommand::Sftp)),
            tool_separator(),
            text_input("Enter host <Alt+R>", &app.profile_host)
                .on_input(Message::ProfileHostChanged)
                .on_submit(Message::ConnectSelectedProfile)
                .padding([3, 6])
                .style(|theme, status| toolbar_input_style(theme, status))
                .width(Length::Fixed(210.0)),
            button("Connect")
                .padding([3, 10])
                .style(|_theme, status| toolbar_action_button_style(status))
                .on_press(Message::ConnectSelectedProfile),
            text(form_endpoint(app)).size(11).color(muted_text()),
            Space::new().width(Fill),
            text(if form_matches_selected_profile(app) {
                "saved"
            } else {
                "modified"
            })
            .size(11)
            .color(muted_text()),
        ]
        .spacing(3)
        .align_y(Alignment::Center),
    )
    .padding([3, 7])
    .height(30)
    .width(Fill)
    .style(|_theme| toolbar_style())
    .into()
}

fn tool_button(label: &'static str, message: Message) -> Element<'static, Message> {
    button(text(label).size(14))
        .width(Length::Fixed(24.0))
        .height(Length::Fixed(22.0))
        .padding(0)
        .style(|_theme, status| toolbar_icon_button_style(status))
        .on_press(message)
        .into()
}

fn tool_separator() -> Element<'static, Message> {
    container(
        Space::new()
            .width(Length::Fixed(1.0))
            .height(Length::Fixed(20.0)),
    )
    .style(|_theme| toolbar_separator_style())
    .width(Length::Fixed(5.0))
    .into()
}

fn form_endpoint(app: &AditApp) -> String {
    let username = app.profile_username.trim();
    let host = app.profile_host.trim();
    let port = app.profile_port.trim();

    if username.is_empty() || host.is_empty() || port.is_empty() {
        String::from("会话信息不完整")
    } else {
        format!("{username}@{host}:{port}")
    }
}

fn form_matches_selected_profile(app: &AditApp) -> bool {
    let Some(profile_id) = app.selected_profile else {
        return false;
    };
    let Some(profile) = app.manager.profile(profile_id) else {
        return false;
    };

    profile.folder == app.profile_folder.trim()
        && profile.name == app.profile_name.trim()
        && profile.host == app.profile_host.trim()
        && profile.port.to_string() == app.profile_port.trim()
        && profile.username == app.profile_username.trim()
        && profile.auth_method == app.profile_auth_method
        && profile.identity_file == app.profile_identity_file.trim()
}

fn sidebar(app: &AditApp) -> Element<'_, Message> {
    let mut sorted_profiles = app.manager.profiles().to_vec();
    sorted_profiles.sort_by(profile_sidebar_order);

    let filter = app.session_filter.trim().to_ascii_lowercase();
    let sorted_profiles = sorted_profiles
        .into_iter()
        .filter(|profile| profile_matches_filter(profile, &filter))
        .collect::<Vec<_>>();
    let profile_count = sorted_profiles.len();
    let mut profiles = column![tree_root_row(profile_count)].spacing(1).width(Fill);
    let mut current_folder = String::new();

    for profile in sorted_profiles {
        if profile.folder != current_folder {
            current_folder = profile.folder.clone();
            profiles = profiles.push(tree_folder_row(current_folder.clone()));
        }

        let selected = Some(profile.id) == app.selected_profile;
        profiles = profiles.push(tree_profile_row(profile, selected));
    }

    let error = app
        .last_error
        .as_ref()
        .map(|message| {
            container(
                row![
                    text(message)
                        .size(12)
                        .color(Color::from_rgb8(255, 173, 173)),
                    Space::new().width(Fill),
                    button("x").on_press(Message::ClearError),
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
                text("Session Manager").size(13).color(Color::WHITE),
                Space::new().width(Fill),
                text("▣").size(12).color(Color::WHITE),
                text("×").size(12).color(Color::WHITE),
            ]
            .spacing(9)
            .align_y(Alignment::Center),
        )
        .height(Length::Fixed(26.0))
        .padding([3, 8])
        .style(|_theme| sidebar_header_style()),
        row![
            sidebar_tool_button("↯", Message::ConnectSelectedProfile),
            sidebar_tool_button("▣", Message::OpenSelectedProfile),
            sidebar_tool_button("+", Message::NewProfileDraft),
            sidebar_tool_button("□", Message::SaveProfile),
            sidebar_tool_button("×", Message::DeleteSelectedProfile),
            sidebar_tool_button("↑", Message::MoveSelectedProfile(ProfileMove::Up)),
            sidebar_tool_button("↓", Message::MoveSelectedProfile(ProfileMove::Down)),
            sidebar_tool_button("A", Message::SortProfiles(ProfileSortKey::Name)),
            sidebar_tool_button("H", Message::SortProfiles(ProfileSortKey::Host)),
            sidebar_tool_button("*", Message::RunMenu(MenuCommand::Logging)),
            Space::new().width(Fill),
        ]
        .padding([3, 5])
        .spacing(3)
        .align_y(Alignment::Center),
        text_input("Filter by folder/session name <Alt+I>", &app.session_filter)
            .on_input(Message::SessionFilterChanged)
            .padding([4, 6])
            .style(|theme, status| toolbar_input_style(theme, status)),
        scrollable(profiles).height(Fill),
        profile_editor(app),
    ]
    .spacing(0)
    .height(Fill)
    .width(Length::Fixed(SIDEBAR_WIDTH));

    if let Some(error) = error {
        content = content.push(error);
    }

    container(content)
        .height(Fill)
        .style(|_theme| sidebar_style())
        .into()
}

fn tree_root_row(profile_count: usize) -> Element<'static, Message> {
    container(
        row![
            text("▾").size(12).color(primary_text()),
            text("▣").size(12).color(folder_color()),
            text("Sessions").size(13).color(primary_text()),
            Space::new().width(Fill),
            text(profile_count.to_string()).size(11).color(muted_text()),
        ]
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .padding([2, 4])
    .width(Fill)
    .into()
}

fn tree_folder_row(folder: String) -> Element<'static, Message> {
    container(
        row![
            Space::new().width(Length::Fixed(14.0)),
            text("▾").size(12).color(primary_text()),
            text("▣").size(12).color(folder_color()),
            text(folder).size(13).color(primary_text()),
        ]
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .padding([2, 4])
    .width(Fill)
    .into()
}

fn tree_profile_row(profile: ConnectionProfile, selected: bool) -> Element<'static, Message> {
    button(
        row![
            Space::new().width(Length::Fixed(34.0)),
            text("▣").size(11).color(session_icon_color()),
            text(profile.name.clone()).size(13).color(primary_text()),
            Space::new().width(Fill),
            text(profile.auth_method.label())
                .size(10)
                .color(muted_text()),
        ]
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .width(Fill)
    .padding([2, 4])
    .style(move |_theme, status| tree_item_style(selected, status))
    .on_press(Message::SelectProfile(profile.id))
    .into()
}

fn profile_matches_filter(profile: &ConnectionProfile, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }

    profile.folder.to_ascii_lowercase().contains(filter)
        || profile.name.to_ascii_lowercase().contains(filter)
        || profile.host.to_ascii_lowercase().contains(filter)
        || profile.username.to_ascii_lowercase().contains(filter)
        || profile.endpoint().to_ascii_lowercase().contains(filter)
}

fn profile_sidebar_order(
    left: &ConnectionProfile,
    right: &ConnectionProfile,
) -> std::cmp::Ordering {
    left.folder
        .cmp(&right.folder)
        .then_with(|| left.sort_order.cmp(&right.sort_order))
        .then_with(|| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
        })
        .then_with(|| left.host.cmp(&right.host))
}

fn sidebar_tool_button(label: &'static str, message: Message) -> Element<'static, Message> {
    button(text(label).size(13))
        .width(Length::Fixed(24.0))
        .height(Length::Fixed(22.0))
        .padding(0)
        .style(|_theme, status| sidebar_tool_button_style(status))
        .on_press(message)
        .into()
}

fn profile_editor(app: &AditApp) -> Element<'_, Message> {
    container(
        column![
            row![
                text("Properties").size(12).color(primary_text()),
                Space::new().width(Fill),
                text(if form_matches_selected_profile(app) {
                    "saved"
                } else {
                    "modified"
                })
                .size(10)
                .color(muted_text()),
            ]
            .spacing(4)
            .align_y(Alignment::Center),
            row![
                text_input("Folder", &app.profile_folder)
                    .on_input(Message::ProfileFolderChanged)
                    .padding([4, 6])
                    .style(|theme, status| text_input_style(theme, status))
                    .width(Length::FillPortion(1)),
                text_input("Name", &app.profile_name)
                    .on_input(Message::ProfileNameChanged)
                    .padding([4, 6])
                    .style(|theme, status| text_input_style(theme, status))
                    .width(Length::FillPortion(1)),
            ]
            .spacing(5),
            row![
                text_input("Host", &app.profile_host)
                    .on_input(Message::ProfileHostChanged)
                    .padding([4, 6])
                    .style(|theme, status| text_input_style(theme, status))
                    .width(Length::FillPortion(2)),
                text_input("Port", &app.profile_port)
                    .on_input(Message::ProfilePortChanged)
                    .padding([4, 6])
                    .style(|theme, status| text_input_style(theme, status))
                    .width(Length::FillPortion(1)),
            ]
            .spacing(5),
            row![
                text_input("User", &app.profile_username)
                    .on_input(Message::ProfileUsernameChanged)
                    .padding([4, 6])
                    .style(|theme, status| text_input_style(theme, status))
                    .width(Length::FillPortion(1)),
                text_input("Password / passphrase", &app.password)
                    .secure(true)
                    .on_input(Message::PasswordChanged)
                    .on_submit(Message::ConnectSelectedProfile)
                    .padding([4, 6])
                    .style(|theme, status| text_input_style(theme, status))
                    .width(Length::FillPortion(1)),
            ]
            .spacing(5),
            row![
                auth_method_button(app, AuthMethod::Auto),
                auth_method_button(app, AuthMethod::Password),
                auth_method_button(app, AuthMethod::Key),
                auth_method_button(app, AuthMethod::Agent),
            ]
            .spacing(4),
            text_input("Identity file", &app.profile_identity_file)
                .on_input(Message::ProfileIdentityFileChanged)
                .padding([4, 6])
                .style(|theme, status| text_input_style(theme, status)),
            row![
                button("Connect")
                    .width(Fill)
                    .padding([5, 8])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::ConnectSelectedProfile),
                button("Save")
                    .width(Fill)
                    .padding([5, 8])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::SaveProfile),
                button("Demo")
                    .width(Fill)
                    .padding([5, 8])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::OpenSelectedProfile),
            ]
            .spacing(5),
        ]
        .spacing(5),
    )
    .padding(7)
    .style(|_theme| properties_panel_style())
    .into()
}

fn auth_method_button(app: &AditApp, auth_method: AuthMethod) -> Element<'static, Message> {
    let selected = app.profile_auth_method == auth_method;

    button(text(auth_method.label()).size(11))
        .padding([4, 6])
        .style(move |_theme, status| method_button_style(selected, status))
        .on_press(Message::ProfileAuthMethodChanged(auth_method))
        .into()
}

fn workspace(app: &AditApp) -> Element<'_, Message> {
    let tabs = app
        .manager
        .sessions()
        .into_iter()
        .fold(row![].spacing(0).height(30), |tabs, session| {
            tabs.push(tab_button(session, app.manager.active_session()))
        });

    let snapshot = active_terminal_snapshot(app);

    container(
        column![
            row![
                scrollable(tabs).direction(scrollable::Direction::Horizontal(
                    scrollable::Scrollbar::new()
                )),
                container(text(app.manager.status_line()).size(12).color(muted_text()))
                    .padding([0, 8])
                    .center_y(30),
            ]
            .align_y(Alignment::Center)
            .height(30)
            .width(Fill),
            mouse_area(terminal_view(
                snapshot,
                app.terminal_focused,
                app.terminal_selection,
                app.terminal_context_menu,
                app.terminal_scroll_offset,
            ))
            .on_press(Message::BeginTerminalSelection)
            .on_release(Message::EndTerminalSelection)
            .on_right_press(Message::ShowTerminalContextMenu)
            .on_move(Message::TerminalPointerMoved)
            .on_scroll(Message::TerminalScrolled)
            .interaction(mouse::Interaction::Text),
        ]
        .height(Fill)
        .width(Fill),
    )
    .padding(0)
    .style(|_theme| workspace_style())
    .height(Fill)
    .width(Fill)
    .into()
}

fn tab_button(
    session: SessionSummary,
    active_session: Option<SessionId>,
) -> Element<'static, Message> {
    let active = Some(session.id) == active_session;

    row![
        button(
            row![
                text("●").size(11).color(status_color(session.status)),
                text(session.title).size(12).color(primary_text()),
            ]
            .spacing(5)
            .align_y(Alignment::Center),
        )
        .padding([5, 9])
        .style(move |_theme, status| tab_button_style(active, status))
        .on_press(Message::ActivateSession(session.id)),
        button("x")
            .padding([5, 7])
            .style(|_theme, status| close_button_style(status))
            .on_press(Message::CloseSession(session.id)),
    ]
    .align_y(Alignment::Center)
    .into()
}

fn terminal_view(
    snapshot: TerminalSnapshot,
    focused: bool,
    selection: Option<TerminalSelection>,
    context_menu: bool,
    _scroll_offset: usize,
) -> Element<'static, Message> {
    let lines = if snapshot.lines.is_empty() {
        column![text("not connected")
            .size(13)
            .font(Font::MONOSPACE)
            .color(default_foreground())]
    } else {
        snapshot
            .lines
            .into_iter()
            .enumerate()
            .fold(column![].spacing(0), |column, (row_index, line)| {
                column.push(terminal_line(line, row_index, selection))
            })
    };

    let mut body = column![].spacing(0);

    if context_menu {
        body = body.push(terminal_context_menu());
    }

    body = body.push(container(lines).height(Fill).width(Fill));

    container(body)
        .padding(TERMINAL_PANEL_PADDING as u16)
        .height(Fill)
        .width(Fill)
        .style(move |_theme| terminal_panel_style(focused))
        .into()
}

fn terminal_context_menu() -> Element<'static, Message> {
    container(
        row![
            button("Copy")
                .padding([7, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::CopyTerminalSelection),
            button("Paste")
                .padding([7, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::PasteIntoTerminal),
            button("Clear")
                .padding([7, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::ClearActiveTerminal),
            button("Bottom")
                .padding([7, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::TerminalJumpToBottom),
            Space::new().width(Fill),
            button("Close")
                .padding([7, 10])
                .style(|_theme, status| close_button_style(status))
                .on_press(Message::HideTerminalContextMenu),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding([7, 9])
    .style(|_theme| terminal_menu_style())
    .into()
}

fn terminal_line(
    line: TerminalLine,
    row_index: usize,
    selection: Option<TerminalSelection>,
) -> Element<'static, Message> {
    if line.cells.is_empty() {
        // Preserve the row height of a visually blank terminal line.
        return text(" ").size(13).font(Font::MONOSPACE).into();
    }

    let selected_range =
        selection.and_then(|selection| selection_range_for_row(selection, row_index));
    let mut col = 0_usize;
    let mut row_widget = row![].spacing(0);

    for cell in line.cells {
        let fg = term_color(cell.fg, default_foreground());
        let font = Font {
            weight: if cell.bold {
                Weight::Bold
            } else {
                Weight::Normal
            },
            ..Font::MONOSPACE
        };

        for ch in cell.text.chars() {
            let selected = selected_range.is_some_and(|range| col >= range.0 && col < range.1);
            let label = text(ch.to_string()).size(13).font(font).color(if selected {
                Color::from_rgb8(245, 249, 255)
            } else {
                fg
            });

            let background = if selected {
                Some(selection_background())
            } else {
                match cell.bg {
                    TermColor::Default => None,
                    other => Some(term_color(other, default_foreground())),
                }
            };

            row_widget = if let Some(background) = background {
                row_widget.push(container(label).style(move |_theme| container::Style {
                    background: Some(Background::Color(background)),
                    ..container::Style::default()
                }))
            } else {
                row_widget.push(label)
            };

            col += 1;
        }
    }

    row_widget.into()
}

fn default_foreground() -> Color {
    Color::from_rgb8(220, 226, 235)
}

/// Resolve an Adit terminal color into a concrete iced color, using `fallback`
/// for the theme default and the xterm 256-color palette for indexed colors.
fn term_color(color: TermColor, fallback: Color) -> Color {
    match color {
        TermColor::Default => fallback,
        TermColor::Rgb(r, g, b) => Color::from_rgb8(r, g, b),
        TermColor::Indexed(index) => palette_color(index),
    }
}

/// The standard xterm 256-color palette: 16 named colors, a 6x6x6 RGB cube, and
/// a 24-step grayscale ramp.
fn palette_color(index: u8) -> Color {
    const NAMED: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];

    match index {
        0..=15 => {
            let (r, g, b) = NAMED[index as usize];
            Color::from_rgb8(r, g, b)
        }
        16..=231 => {
            let c = index - 16;
            let level = |v: u8| -> u8 {
                if v == 0 {
                    0
                } else {
                    55 + v * 40
                }
            };
            Color::from_rgb8(level(c / 36), level((c / 6) % 6), level(c % 6))
        }
        232..=255 => {
            let value = 8 + (index - 232) * 10;
            Color::from_rgb8(value, value, value)
        }
    }
}

fn status_bar(app: &AditApp) -> Element<'_, Message> {
    let status = if let Some(error) = &app.last_error {
        format!("Error: {error}")
    } else {
        app.notice.clone()
    };

    container(
        row![
            text(status).size(12).color(muted_text()),
            Space::new().width(Fill),
            text(app.manager.status_line()).size(12).color(muted_text()),
            Space::new().width(Length::Fixed(18.0)),
            text(format!("Profiles: {}", app.manager.profiles().len()))
                .size(12)
                .color(muted_text()),
            Space::new().width(Length::Fixed(18.0)),
            text(format!(
                "{}x{}",
                app.terminal_size.cols, app.terminal_size.rows
            ))
            .size(12)
            .color(muted_text()),
            Space::new().width(Length::Fixed(18.0)),
            text("Adit Native").size(12).color(muted_text()),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding([6, 14])
    .height(30)
    .width(Fill)
    .style(|_theme| status_bar_style())
    .into()
}

fn muted_text() -> Color {
    Color::from_rgb8(92, 96, 102)
}

fn primary_text() -> Color {
    Color::from_rgb8(17, 24, 32)
}

fn app_background() -> Color {
    Color::from_rgb8(210, 214, 219)
}

fn panel_background_hover() -> Color {
    Color::from_rgb8(226, 235, 247)
}

fn field_background() -> Color {
    Color::from_rgb8(255, 255, 255)
}

fn terminal_background() -> Color {
    Color::from_rgb8(0, 0, 0)
}

fn selection_background() -> Color {
    Color::from_rgb8(20, 96, 180)
}

fn border_color() -> Color {
    Color::from_rgb8(181, 187, 195)
}

fn border_strong() -> Color {
    Color::from_rgb8(85, 135, 195)
}

fn accent() -> Color {
    Color::from_rgb8(0, 120, 215)
}

fn accent_hover() -> Color {
    Color::from_rgb8(26, 140, 232)
}

fn accent_pressed() -> Color {
    Color::from_rgb8(0, 93, 170)
}

fn danger() -> Color {
    Color::from_rgb8(214, 48, 49)
}

fn danger_background() -> Color {
    Color::from_rgb8(255, 235, 235)
}

fn folder_color() -> Color {
    Color::from_rgb8(232, 169, 46)
}

fn session_icon_color() -> Color {
    Color::from_rgb8(82, 88, 96)
}

fn transparent() -> Color {
    Color {
        a: 0.0,
        ..Color::BLACK
    }
}

fn border(radius: f32, width: f32, color: Color) -> Border {
    Border {
        radius: radius.into(),
        width,
        color,
    }
}

fn app_background_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(app_background())),
        text_color: Some(primary_text()),
        ..container::Style::default()
    }
}

fn top_bar_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb8(250, 250, 250))),
        text_color: Some(primary_text()),
        border: border(0.0, 1.0, Color::from_rgb8(198, 202, 207)),
        ..container::Style::default()
    }
}

fn menu_panel_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb8(247, 247, 247))),
        text_color: Some(primary_text()),
        border: border(0.0, 1.0, Color::from_rgb8(198, 202, 207)),
        ..container::Style::default()
    }
}

fn toolbar_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb8(238, 240, 243))),
        text_color: Some(primary_text()),
        border: border(0.0, 1.0, Color::from_rgb8(187, 192, 198)),
        ..container::Style::default()
    }
}

fn sidebar_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb8(252, 252, 252))),
        text_color: Some(primary_text()),
        border: border(0.0, 1.0, Color::from_rgb8(150, 157, 166)),
        ..container::Style::default()
    }
}

fn workspace_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb8(232, 234, 237))),
        text_color: Some(primary_text()),
        ..container::Style::default()
    }
}

fn terminal_panel_style(focused: bool) -> container::Style {
    container::Style {
        background: Some(Background::Color(terminal_background())),
        text_color: Some(default_foreground()),
        border: border(
            0.0,
            1.0,
            if focused {
                accent()
            } else {
                Color::from_rgb8(55, 55, 55)
            },
        ),
        ..container::Style::default()
    }
}

fn terminal_menu_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb8(246, 247, 249))),
        text_color: Some(primary_text()),
        border: border(1.0, 1.0, border_color()),
        ..container::Style::default()
    }
}

fn status_bar_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb8(238, 240, 243))),
        text_color: Some(muted_text()),
        border: border(0.0, 1.0, Color::from_rgb8(187, 192, 198)),
        ..container::Style::default()
    }
}

fn error_panel_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(danger_background())),
        text_color: Some(primary_text()),
        border: border(0.0, 1.0, danger()),
        ..container::Style::default()
    }
}

fn text_input_style(_theme: &Theme, status: text_input::Status) -> text_input::Style {
    let border_color = match status {
        text_input::Status::Focused { .. } => accent(),
        text_input::Status::Hovered => border_strong(),
        text_input::Status::Active | text_input::Status::Disabled => border_color(),
    };

    text_input::Style {
        background: Background::Color(field_background()),
        border: border(0.0, 1.0, border_color),
        icon: muted_text(),
        placeholder: muted_text(),
        value: primary_text(),
        selection: Color::from_rgb8(188, 216, 244),
    }
}

fn base_button_style(background: Color, text_color: Color, border_color: Color) -> button::Style {
    button::Style {
        background: Some(Background::Color(background)),
        text_color,
        border: border(1.0, 1.0, border_color),
        ..button::Style::default()
    }
}

fn primary_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => accent_hover(),
        button::Status::Pressed => accent_pressed(),
        button::Status::Disabled => Color::from_rgb8(220, 224, 229),
        button::Status::Active => accent(),
    };
    base_button_style(background, Color::WHITE, background)
}

fn secondary_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => panel_background_hover(),
        button::Status::Pressed => Color::from_rgb8(210, 222, 239),
        button::Status::Disabled => Color::from_rgb8(232, 234, 237),
        button::Status::Active => Color::from_rgb8(246, 247, 249),
    };
    base_button_style(background, primary_text(), border_color())
}

fn method_button_style(selected: bool, status: button::Status) -> button::Style {
    let background = match (selected, status) {
        (true, button::Status::Pressed) => accent_pressed(),
        (true, button::Status::Hovered) => accent_hover(),
        (true, _) => Color::from_rgb8(210, 230, 250),
        (false, button::Status::Hovered) => panel_background_hover(),
        (false, button::Status::Pressed) => Color::from_rgb8(218, 225, 234),
        _ => field_background(),
    };
    let border_color = if selected { accent() } else { border_color() };
    base_button_style(
        background,
        if selected && matches!(status, button::Status::Pressed | button::Status::Hovered) {
            Color::WHITE
        } else {
            primary_text()
        },
        border_color,
    )
}

fn menu_button_style(active: bool, status: button::Status) -> button::Style {
    let background = match (active, status) {
        (true, _) => Color::from_rgb8(223, 235, 249),
        (false, button::Status::Hovered) => Color::from_rgb8(232, 239, 249),
        (false, button::Status::Pressed) => Color::from_rgb8(214, 226, 241),
        _ => transparent(),
    };
    base_button_style(background, primary_text(), transparent())
}

fn tab_button_style(active: bool, status: button::Status) -> button::Style {
    let background = match (active, status) {
        (true, _) => Color::from_rgb8(255, 255, 255),
        (false, button::Status::Hovered) => Color::from_rgb8(244, 247, 251),
        (false, button::Status::Pressed) => Color::from_rgb8(226, 232, 241),
        _ => Color::from_rgb8(232, 234, 237),
    };
    let border_color = if active {
        Color::from_rgb8(140, 146, 153)
    } else {
        border_color()
    };
    base_button_style(background, primary_text(), border_color)
}

fn close_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => danger_background(),
        button::Status::Pressed => Color::from_rgb8(255, 214, 214),
        _ => transparent(),
    };
    base_button_style(background, muted_text(), transparent())
}

fn menu_command_button_style(status: button::Status) -> button::Style {
    secondary_button_style(status)
}

fn toolbar_icon_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => Color::from_rgb8(219, 231, 247),
        button::Status::Pressed => Color::from_rgb8(199, 218, 243),
        _ => transparent(),
    };
    base_button_style(background, primary_text(), transparent())
}

fn toolbar_action_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => Color::from_rgb8(219, 231, 247),
        button::Status::Pressed => Color::from_rgb8(199, 218, 243),
        _ => Color::from_rgb8(247, 248, 250),
    };
    base_button_style(background, primary_text(), border_color())
}

fn toolbar_separator_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb8(174, 180, 188))),
        ..container::Style::default()
    }
}

fn toolbar_input_style(_theme: &Theme, status: text_input::Status) -> text_input::Style {
    let border_color = match status {
        text_input::Status::Focused { .. } => accent(),
        text_input::Status::Hovered => border_strong(),
        text_input::Status::Active | text_input::Status::Disabled => {
            Color::from_rgb8(160, 166, 174)
        }
    };

    text_input::Style {
        background: Background::Color(Color::WHITE),
        border: border(0.0, 1.0, border_color),
        icon: muted_text(),
        placeholder: Color::from_rgb8(115, 120, 126),
        value: primary_text(),
        selection: Color::from_rgb8(188, 216, 244),
    }
}

fn sidebar_header_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb8(80, 98, 118))),
        text_color: Some(Color::WHITE),
        border: border(0.0, 1.0, Color::from_rgb8(70, 84, 101)),
        ..container::Style::default()
    }
}

fn sidebar_tool_button_style(status: button::Status) -> button::Style {
    toolbar_icon_button_style(status)
}

fn tree_item_style(selected: bool, status: button::Status) -> button::Style {
    let background = match (selected, status) {
        (true, _) => Color::from_rgb8(207, 207, 207),
        (false, button::Status::Hovered) => Color::from_rgb8(232, 239, 249),
        (false, button::Status::Pressed) => Color::from_rgb8(218, 228, 242),
        _ => transparent(),
    };
    let border_color = if selected {
        Color::from_rgb8(154, 154, 154)
    } else {
        transparent()
    };
    base_button_style(background, primary_text(), border_color)
}

fn properties_panel_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb8(246, 247, 249))),
        text_color: Some(primary_text()),
        border: border(0.0, 1.0, Color::from_rgb8(185, 190, 197)),
        ..container::Style::default()
    }
}

fn status_color(status: SessionStatus) -> Color {
    match status {
        SessionStatus::Connecting => Color::from_rgb8(247, 190, 84),
        SessionStatus::Connected => Color::from_rgb8(76, 208, 137),
        SessionStatus::Disconnected => muted_text(),
        SessionStatus::Error => danger(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::key::{Code, Physical};

    fn key_press(
        key: Key,
        modified_key: Key,
        physical_key: Physical,
        modifiers: keyboard::Modifiers,
        text: Option<&str>,
    ) -> keyboard::Event {
        keyboard::Event::KeyPressed {
            key,
            modified_key,
            physical_key,
            location: keyboard::Location::Standard,
            modifiers,
            text: text.map(Into::into),
            repeat: false,
        }
    }

    #[test]
    fn encodes_regular_text() {
        let event = key_press(
            Key::Character("a".into()),
            Key::Character("a".into()),
            Physical::Code(Code::KeyA),
            keyboard::Modifiers::empty(),
            Some("a"),
        );

        assert_eq!(encode_keyboard_event(event), Some(b"a".to_vec()));
    }

    #[test]
    fn encodes_ctrl_c() {
        let event = key_press(
            Key::Character("c".into()),
            Key::Character("c".into()),
            Physical::Code(Code::KeyC),
            keyboard::Modifiers::CTRL,
            None,
        );

        assert_eq!(encode_keyboard_event(event), Some(vec![0x03]));
    }

    #[test]
    fn ctrl_shift_c_is_terminal_copy_shortcut() {
        let event = key_press(
            Key::Character("c".into()),
            Key::Character("C".into()),
            Physical::Code(Code::KeyC),
            keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
            None,
        );

        assert!(is_terminal_copy_shortcut(&event));
    }

    #[test]
    fn paste_normalizes_newlines_for_pty() {
        assert_eq!(normalize_paste("one\r\ntwo\n"), b"one\rtwo\r".to_vec());
    }

    #[test]
    fn selection_extracts_text_across_rows() {
        let snapshot = TerminalSnapshot {
            title: String::from("test"),
            size: TerminalSize::new(10, 3),
            first_row: 0,
            total_rows: 3,
            lines: vec![
                TerminalLine::plain("alpha"),
                TerminalLine::plain("bravo"),
                TerminalLine::plain("charlie"),
            ],
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
        };
        let selection = TerminalSelection {
            start: TerminalPoint { row: 0, col: 2 },
            end: TerminalPoint { row: 2, col: 4 },
        };

        assert_eq!(selection_to_text(&snapshot, selection), "pha\nbravo\nchar");
    }

    #[test]
    fn scroll_delta_converts_to_terminal_rows() {
        assert_eq!(
            scroll_delta_to_rows(mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 }),
            Some(3)
        );
        assert_eq!(
            scroll_delta_to_rows(mouse::ScrollDelta::Pixels {
                x: 0.0,
                y: -TERMINAL_ROW_HEIGHT
            }),
            Some(-1)
        );
    }

    #[test]
    fn shift_page_keys_are_local_terminal_scroll_shortcuts() {
        let page_up = key_press(
            Key::Named(Named::PageUp),
            Key::Named(Named::PageUp),
            Physical::Code(Code::PageUp),
            keyboard::Modifiers::SHIFT,
            None,
        );
        let page_down = key_press(
            Key::Named(Named::PageDown),
            Key::Named(Named::PageDown),
            Physical::Code(Code::PageDown),
            keyboard::Modifiers::SHIFT,
            None,
        );

        assert_eq!(
            terminal_scroll_shortcut(&page_up, 28),
            Some(TerminalScrollAction::Lines(27))
        );
        assert_eq!(
            terminal_scroll_shortcut(&page_down, 28),
            Some(TerminalScrollAction::Lines(-27))
        );
    }

    #[test]
    fn selection_range_handles_reversed_drag() {
        let selection = TerminalSelection {
            start: TerminalPoint { row: 3, col: 8 },
            end: TerminalPoint { row: 1, col: 2 },
        };

        assert_eq!(selection_range_for_row(selection, 1), Some((2, usize::MAX)));
        assert_eq!(selection_range_for_row(selection, 2), Some((0, usize::MAX)));
        assert_eq!(selection_range_for_row(selection, 3), Some((0, 8)));
    }

    #[test]
    fn encodes_arrow_keys() {
        let event = key_press(
            Key::Named(Named::ArrowUp),
            Key::Named(Named::ArrowUp),
            Physical::Code(Code::ArrowUp),
            keyboard::Modifiers::empty(),
            None,
        );

        assert_eq!(encode_keyboard_event(event), Some(b"\x1b[A".to_vec()));
    }

    #[test]
    fn encodes_alt_text_with_escape_prefix() {
        let event = key_press(
            Key::Character("x".into()),
            Key::Character("x".into()),
            Physical::Code(Code::KeyX),
            keyboard::Modifiers::ALT,
            Some("x"),
        );

        assert_eq!(encode_keyboard_event(event), Some(b"\x1bx".to_vec()));
    }
}
