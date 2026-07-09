use adit_domain::{
    AuthMethod, ConnectionProfile, ProfileId, Protocol, SessionId, SessionStatus, TunnelDef,
};
use adit_session::{
    HostKeyPromptInfo, LocalEntry, ProfileDropPosition, ProfileMove, ProfileSortKey, SessionManager,
    SessionSummary, SftpBrowser, SftpEntry, TransferDirection, TransferItem, TransferStatus,
    TunnelKind, TunnelState,
};
use adit_storage::{
    AppSettings, CredentialStore, ProfileCatalog, ProfileStore, SettingsStore, Snippet,
};
use adit_terminal::{
    Color as TermColor, MouseMode, TerminalLine, TerminalSize, TerminalSnapshot, Viewport,
};
use iced::font::Weight;
use iced::keyboard::{self, key::Named, Key};
use iced::widget::{
    button, checkbox, column, container, mouse_area, opaque, progress_bar, row, scrollable, stack,
    text, text_input, tooltip, Space,
};
use iced::{
    clipboard, event, mouse, window, Alignment, Background, Border, Color, Element, Fill, Font,
    Length, Point, Shadow, Subscription, Task, Theme, Vector,
};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::time::Instant;

/// Which SFTP pane a row belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SftpPane {
    Local,
    Remote,
}

/// Column to sort an SFTP pane's listing by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SftpSortKey {
    Name,
    Size,
    Modified,
}

/// Whether the UI is currently painting in dark mode. Set once per frame at the
/// top of `view` so the palette token fns can resolve light/dark without every
/// `.style` closure having to thread the theme through.
static DARK_MODE: AtomicBool = AtomicBool::new(false);

/// Terminal appearance, set once per frame at the top of `view` (like
/// [`DARK_MODE`]) so the deep terminal render/hit-test/color fns can read the
/// active font + palette without threading them through every call.
static TERM_FONT: AtomicU8 = AtomicU8::new(0);
static TERM_FONT_SIZE: AtomicU32 = AtomicU32::new(13);
static TERM_SCHEME: AtomicU8 = AtomicU8::new(0);

fn is_dark() -> bool {
    DARK_MODE.load(Ordering::Relaxed)
}
use std::{collections::BTreeSet, time::Duration};

pub struct AditApp {
    manager: SessionManager,
    profile_store: ProfileStore,
    credential_store: CredentialStore,
    selected_profile: Option<ProfileId>,
    hovered_profile: Option<ProfileId>,
    dragged_profile: Option<ProfileId>,
    // Set once a live drag has actually reordered rows, so a plain click (which
    // also arms a drag) doesn't needlessly re-persist profiles on release.
    profile_drag_moved: bool,
    // Sidebar-relative cursor position while a profile drag is in flight, so the
    // floating "ghost" card can follow the pointer.
    profile_drag_cursor: Option<Point>,
    group_drop_target: Option<String>,
    group_context_menu: Option<String>,
    editing_group: Option<String>,
    group_name_draft: String,
    profile_context_menu: Option<ProfileId>,
    profile_editor: Option<ProfileId>,
    connection_dialog: Option<ConnectionDialog>,
    groups: BTreeSet<String>,
    collapsed_groups: BTreeSet<String>,
    active_menu: Option<MenuKind>,
    profile_group: String,
    profile_name: String,
    profile_host: String,
    profile_port: String,
    profile_username: String,
    profile_auth_method: AuthMethod,
    profile_protocol: Protocol,
    profile_identity_file: String,
    profile_startup_command: String,
    profile_terminal_type: String,
    connect_timeout_secs: u32,
    scrollback_lines: u32,
    snippets: Vec<Snippet>,
    snippets_open: bool,
    snippet_name_draft: String,
    snippet_command_draft: String,
    auto_check_updates: bool,
    password: String,
    remember_connection_password: bool,
    session_filter: String,
    sftp_upload_path: String,
    sftp_new_folder: String,
    sftp_rename: Option<(SftpPane, String)>,
    sftp_rename_to: String,
    sftp_delete_target: Option<(SftpPane, String, bool)>,
    sftp_local_path_edit: String,
    sftp_remote_path_edit: String,
    sftp_local_cwd_seen: String,
    sftp_remote_cwd_seen: String,
    sftp_local_selected: BTreeSet<String>,
    sftp_remote_selected: BTreeSet<String>,
    sftp_local_sort: (SftpSortKey, bool),
    sftp_remote_sort: (SftpSortKey, bool),
    sftp_last_click: Option<(SftpPane, String, Instant)>,
    sftp_drag: Option<(SftpPane, String)>,
    sftp_drag_over: Option<SftpPane>,
    sftp_drag_cursor: Option<Point>,
    tunnels_open: bool,
    about_open: bool,
    tunnel_kind: TunnelKind,
    tunnel_bind_addr: String,
    tunnel_bind_port: String,
    tunnel_target_host: String,
    tunnel_target_port: String,
    tunnel_save: bool,
    terminal_input: String,
    terminal_focused: bool,
    terminal_size: TerminalSize,
    terminal_pointer: Option<TerminalPoint>,
    terminal_selection: Option<TerminalSelection>,
    terminal_selecting: bool,
    // Last terminal press (cell, time, click-count) for double/triple-click
    // word/line selection.
    terminal_click: Option<(TerminalPoint, Instant, u8)>,
    terminal_context_menu: bool,
    terminal_scroll_offset: usize,
    // Latest keyboard modifier state, so wheel handling can tell a plain scroll
    // from a Ctrl+wheel zoom.
    modifiers: keyboard::Modifiers,
    window_width: f32,
    window_height: f32,
    sidebar_width: f32,
    sidebar_visible: bool,
    sidebar_dragging: bool,
    cursor_pos: Point,
    context_menu_pos: Point,
    dark_mode: bool,
    font_family: String,
    font_size: f32,
    color_scheme: String,
    appearance_open: bool,
    update_dialog_open: bool,
    update_state: UpdateState,
    /// The 选项 (config path + session-log) dialog.
    options_open: bool,
    log_dir: String,
    log_name_pattern: String,
    auto_log_on_connect: bool,
    log_plaintext: bool,
    copy_on_select: bool,
    right_click_paste: bool,
    confirm_multiline_paste: bool,
    pending_paste: Option<String>,
    paste_confirm_open: bool,
    /// Left button held over a mouse-reporting terminal (for drag/release
    /// reports); and the last cell already reported (to dedupe motion events).
    mouse_button_down: bool,
    mouse_report_cell: Option<TerminalPoint>,
    search_open: bool,
    search_query: String,
    search_matches: Vec<SearchMatch>,
    search_index: Option<usize>,
    renaming_session: Option<SessionId>,
    session_rename_draft: String,
    dragged_tab: Option<SessionId>,
    broadcast_input: bool,
    // Bottom command window: a line-oriented send box (SecureCRT-style). The
    // typed text lives in `terminal_input`.
    command_window_open: bool,
    command_target: CommandTarget,
    command_send_immediately: bool,
    command_history: Vec<String>,
    // Cursor into `command_history` while stepping with ▲/▼ (None ⇒ live edit).
    command_history_pos: Option<usize>,
    /// Sessions tiled in the workspace. Empty ⇒ the single-pane view (renders the
    /// active session). 2–4 entries ⇒ split panes. `focused_pane` indexes it and
    /// mirrors the manager's active session.
    panes: Vec<SessionId>,
    focused_pane: usize,
    tile_mode: TileMode,
    settings_store: SettingsStore,
    /// The last settings snapshot written to disk; the Tick loop persists when
    /// the live config drifts from this.
    persisted_settings: AppSettings,
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
    NewGroup,
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
    Tunnels,
    Logging,
    ToggleAutoReconnect,
    Appearance,
    Options,
    ImportSshConfig,
    Snippets,
    ToggleBroadcast,
    ToggleCommandWindow,
    SplitPane,
    TileVertical,
    TileHorizontal,
    TileGrid,
    Untile,
    CheckUpdate,
    About,
}

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    ToggleMenu(MenuKind),
    ToggleTheme,
    OpenAppearance,
    CloseAppearance,
    FontFamilyChanged(u8),
    FontSizeStep(i32),
    ColorSchemeChanged(u8),
    OpenOptions,
    CloseOptions,
    LogDirChanged(String),
    LogNamePatternChanged(String),
    ToggleAutoLog(bool),
    ToggleLogPlaintext(bool),
    ToggleCopyOnSelect(bool),
    ToggleRightClickPaste(bool),
    ToggleConfirmMultilinePaste(bool),
    ConfirmPaste,
    CancelPaste,
    OpenConfigFolder,
    OpenLogFolder,
    ToggleBroadcast,
    RunMenu(MenuCommand),
    SelectProfile(ProfileId),
    ProfilePressed(ProfileId),
    ProfileDoubleClicked(ProfileId),
    ProfileHovered(ProfileId),
    ProfileHoverExited(ProfileId),
    ProfileDragOver(ProfileId, ProfileDropPosition),
    ProfileDropped(ProfileId),
    ProfileDragOverGroup(String),
    ProfileDroppedOnGroup(String),
    ProfileGroupHoverExited(String),
    CancelProfileDrag,
    ShowGroupContextMenu(String),
    HideGroupContextMenu,
    RenameGroupFromContext(String),
    NewProfileInGroup(String),
    DeleteGroupFromContext(String),
    GroupNameDraftChanged(String),
    SaveGroupRename,
    CancelGroupRename,
    ShowProfileContextMenu(ProfileId),
    HideProfileContextMenu,
    SidebarCursorMoved(Point),
    EditProfileFromContext(ProfileId),
    CloseProfileEditor,
    ConnectProfileFromContext(ProfileId),
    CloneProfileFromContext(ProfileId),
    DeleteProfileFromContext(ProfileId),
    ConnectionPasswordChanged(String),
    RememberConnectionPasswordChanged(bool),
    ConfirmConnection,
    CancelConnection,
    RespondHostKey { session_id: SessionId, accept: bool },
    OpenSftp,
    CloseSftp,
    OpenTunnels,
    CloseTunnels,
    CloseAbout,
    TunnelKindChanged(TunnelKind),
    TunnelBindAddrChanged(String),
    TunnelBindPortChanged(String),
    TunnelTargetHostChanged(String),
    TunnelTargetPortChanged(String),
    ToggleTunnelSave(bool),
    AddTunnel,
    CloseTunnel(u64),
    RemoveSavedTunnel(usize),
    SftpNavigate(String),
    SftpUp,
    SftpRefresh,
    SftpLocalNavigate(String),
    SftpLocalUp,
    SftpLocalRefresh,
    SftpUploadLocal(String),
    SftpDownload(String),
    SftpRowPress(SftpPane, String),
    SftpTransferSelected(SftpPane),
    SftpFileDropped(std::path::PathBuf),
    SftpLocalPathChanged(String),
    SftpLocalGo,
    SftpRemotePathChanged(String),
    SftpRemoteGo,
    SftpUploadPathChanged(String),
    SftpUpload,
    SftpPickUpload,
    SftpUploadPicked(Option<std::path::PathBuf>),
    SftpNewFolderChanged(String),
    SftpMkdir,
    SftpBeginRename(SftpPane, String),
    SftpRenameToChanged(String),
    SftpConfirmRename,
    SftpCancelRename,
    SftpBeginDelete(SftpPane, String, bool),
    SftpConfirmDelete,
    SftpCancelDelete,
    SftpSort(SftpPane, SftpSortKey),
    SftpClearTransfers,
    SftpDragEnter(SftpPane),
    SftpDragMove(SftpPane, Point),
    ToggleProfileGroup(String),
    ProfileGroupChanged(String),
    ProfileNameChanged(String),
    ProfileHostChanged(String),
    ProfilePortChanged(String),
    ProfileUsernameChanged(String),
    ProfileAuthMethodChanged(AuthMethod),
    ProfileProtocolChanged(Protocol),
    ProfileIdentityFileChanged(String),
    PickIdentityFile,
    IdentityFilePicked(Option<std::path::PathBuf>),
    ProfileStartupCommandChanged(String),
    ProfileTerminalTypeChanged(String),
    ConnectTimeoutChanged(String),
    ScrollbackLinesChanged(String),
    SessionFilterChanged(String),
    NewProfileDraft,
    NewGroupDraft,
    SaveProfile,
    DeleteSelectedProfile,
    MoveSelectedProfile(ProfileMove),
    SortProfiles(ProfileSortKey),
    TerminalInputChanged(String),
    KeyboardInput(keyboard::Event),
    ModifiersChanged(keyboard::Modifiers),
    WindowResized { width: f32, height: f32 },
    ToggleSidebar,
    BeginSidebarDrag,
    SidebarDragMove(f32),
    EndSidebarDrag,
    FocusTerminal,
    SplitPane,
    ClosePane(usize),
    FocusPane(usize),
    PaneMousePressed(usize),
    PaneRightPressed(usize),
    PanePointerMoved(usize, Point),
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
    RetryActiveSession,
    ActivateSession(SessionId),
    TabPressed(SessionId),
    TabDragOver(SessionId),
    TabReleased,
    CloseSession(SessionId),
    RenameSessionPrompt(SessionId),
    SessionRenameChanged(String),
    ConfirmRenameSession,
    CancelRenameSession,
    DisconnectActive,
    SendTerminalInput,
    ToggleCommandWindow,
    CommandTargetToggled,
    ToggleCommandSendImmediately,
    CommandHistoryPrev,
    CommandHistoryNext,
    ClearActiveTerminal,
    ClearError,
    CloseSnippets,
    SnippetNameChanged(String),
    SnippetCommandChanged(String),
    AddSnippet,
    DeleteSnippet(usize),
    SendSnippet(usize),
    OpenSearch,
    CloseSearch,
    SearchQueryChanged(String),
    SearchNext,
    SearchPrev,
    CheckForUpdates,
    UpdateChecked(Result<Option<UpdateInfo>, String>),
    AutoUpdateChecked(Result<Option<UpdateInfo>, String>),
    ToggleAutoCheckUpdates(bool),
    StartUpdateDownload,
    UpdateDownloaded(Result<String, String>),
    CloseUpdateDialog,
    OpenReleaseNotes(String),
}

/// A newer release discovered by the in-app update check.
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    tag: String,
    installer_url: String,
    installer_name: String,
    notes_url: String,
}

/// State of the in-app updater, surfaced in the update dialog.
#[derive(Debug, Clone, Default)]
enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(UpdateInfo),
    Downloading,
    Launched,
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalPoint {
    row: usize,
    col: usize,
}

/// A scrollback-search hit: an absolute row plus the matched character span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SearchMatch {
    row: usize,
    col: usize,
    len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalSelection {
    start: TerminalPoint,
    end: TerminalPoint,
}

#[derive(Debug, Clone)]
struct ConnectionDialog {
    profile_id: ProfileId,
    title: String,
    endpoint: String,
    auth_method: AuthMethod,
    identity_file: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalScrollAction {
    Lines(i32),
    Top,
    Bottom,
}

// Monospace cell metrics derive from the active font size so changing the size
// rescales the whole grid consistently (render, hit-testing, and size
// estimation all read the same two fns). Ratios chosen so size 13 reproduces
// the previous fixed 7.8 x 17.0 cell.
const CELL_WIDTH_RATIO: f32 = 0.6;
const CELL_HEIGHT_RATIO: f32 = 1.308;
const MIN_FONT_SIZE: u32 = 9;
const MAX_FONT_SIZE: u32 = 28;

/// Active terminal font size in px (the value set on [`TERM_FONT_SIZE`]).
fn term_font_size() -> f32 {
    TERM_FONT_SIZE.load(Ordering::Relaxed) as f32
}

/// Width of one monospace cell at the active font size.
fn cell_width() -> f32 {
    term_font_size() * CELL_WIDTH_RATIO
}

/// Height of one terminal row at the active font size.
fn cell_height() -> f32 {
    term_font_size() * CELL_HEIGHT_RATIO
}

/// Selectable terminal fonts. The first is the system monospace default; the
/// rest are common Windows monospace families resolved by name (a missing
/// family falls back through cosmic-text, never a hard error).
const FONT_PRESETS: &[(&str, Option<&'static str>)] = &[
    ("系统等宽", None),
    ("Consolas", Some("Consolas")),
    ("Cascadia Mono", Some("Cascadia Mono")),
    ("Cascadia Code", Some("Cascadia Code")),
    ("Courier New", Some("Courier New")),
    ("Lucida Console", Some("Lucida Console")),
];

/// The base terminal font (family only; per-cell weight is layered on top).
fn term_font() -> Font {
    let idx = TERM_FONT.load(Ordering::Relaxed) as usize;
    match FONT_PRESETS.get(idx).and_then(|(_, family)| *family) {
        Some(name) => Font::with_name(name),
        None => Font::MONOSPACE,
    }
}

/// Preset index for a persisted font-family display name (0 = system default).
fn font_preset_index(name: &str) -> u8 {
    FONT_PRESETS
        .iter()
        .position(|(display, _)| *display == name)
        .unwrap_or(0) as u8
}

/// A terminal color scheme: window background/foreground, selection highlight,
/// and the 16 ANSI colors (indices 16..=255 use the standard xterm cube/ramp).
struct ColorScheme {
    name: &'static str,
    background: (u8, u8, u8),
    foreground: (u8, u8, u8),
    selection: (u8, u8, u8),
    ansi: [(u8, u8, u8); 16],
}

const COLOR_SCHEMES: &[ColorScheme] = &[
    ColorScheme {
        name: "默认",
        background: (20, 21, 28),
        foreground: (220, 226, 235),
        selection: (22, 92, 84),
        ansi: [
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
        ],
    },
    ColorScheme {
        name: "Dracula",
        background: (40, 42, 54),
        foreground: (248, 248, 242),
        selection: (68, 71, 90),
        ansi: [
            (33, 34, 44),
            (255, 85, 85),
            (80, 250, 123),
            (241, 250, 140),
            (189, 147, 249),
            (255, 121, 198),
            (139, 233, 253),
            (248, 248, 242),
            (98, 114, 164),
            (255, 110, 110),
            (105, 255, 148),
            (255, 255, 165),
            (214, 172, 255),
            (255, 146, 223),
            (164, 255, 255),
            (255, 255, 255),
        ],
    },
    ColorScheme {
        name: "One Dark",
        background: (40, 44, 52),
        foreground: (171, 178, 191),
        selection: (62, 68, 81),
        ansi: [
            (40, 44, 52),
            (224, 108, 117),
            (152, 195, 121),
            (229, 192, 123),
            (97, 175, 239),
            (198, 120, 221),
            (86, 182, 194),
            (171, 178, 191),
            (92, 99, 112),
            (224, 108, 117),
            (152, 195, 121),
            (229, 192, 123),
            (97, 175, 239),
            (198, 120, 221),
            (86, 182, 194),
            (255, 255, 255),
        ],
    },
    ColorScheme {
        name: "Nord",
        background: (46, 52, 64),
        foreground: (216, 222, 233),
        selection: (67, 76, 94),
        ansi: [
            (59, 66, 82),
            (191, 97, 106),
            (163, 190, 140),
            (235, 203, 139),
            (129, 161, 193),
            (180, 142, 173),
            (136, 192, 208),
            (229, 233, 240),
            (76, 86, 106),
            (191, 97, 106),
            (163, 190, 140),
            (235, 203, 139),
            (129, 161, 193),
            (180, 142, 173),
            (143, 188, 187),
            (236, 239, 244),
        ],
    },
    ColorScheme {
        name: "Gruvbox Dark",
        background: (40, 40, 40),
        foreground: (235, 219, 178),
        selection: (80, 73, 69),
        ansi: [
            (40, 40, 40),
            (204, 36, 29),
            (152, 151, 26),
            (215, 153, 33),
            (69, 133, 136),
            (177, 98, 134),
            (104, 157, 106),
            (168, 153, 132),
            (146, 131, 116),
            (251, 73, 52),
            (184, 187, 38),
            (250, 189, 47),
            (131, 165, 152),
            (211, 134, 155),
            (142, 192, 124),
            (235, 219, 178),
        ],
    },
    ColorScheme {
        name: "Solarized Dark",
        background: (0, 43, 54),
        foreground: (131, 148, 150),
        selection: (7, 54, 66),
        ansi: [
            (7, 54, 66),
            (220, 50, 47),
            (133, 153, 0),
            (181, 137, 0),
            (38, 139, 210),
            (211, 54, 130),
            (42, 161, 152),
            (238, 232, 213),
            (0, 43, 54),
            (203, 75, 22),
            (88, 110, 117),
            (101, 123, 131),
            (131, 148, 150),
            (108, 113, 196),
            (147, 161, 161),
            (253, 246, 227),
        ],
    },
    ColorScheme {
        name: "Solarized Light",
        background: (253, 246, 227),
        foreground: (101, 123, 131),
        selection: (238, 232, 213),
        ansi: [
            (7, 54, 66),
            (220, 50, 47),
            (133, 153, 0),
            (181, 137, 0),
            (38, 139, 210),
            (211, 54, 130),
            (42, 161, 152),
            (238, 232, 213),
            (0, 43, 54),
            (203, 75, 22),
            (88, 110, 117),
            (101, 123, 131),
            (131, 148, 150),
            (108, 113, 196),
            (147, 161, 161),
            (253, 246, 227),
        ],
    },
];

/// The active color scheme (defaults to the first if the index is stale).
fn active_scheme() -> &'static ColorScheme {
    let idx = TERM_SCHEME.load(Ordering::Relaxed) as usize;
    &COLOR_SCHEMES[idx.min(COLOR_SCHEMES.len() - 1)]
}

/// Scheme index for a persisted scheme name (0 = default palette).
fn color_scheme_index(name: &str) -> u8 {
    COLOR_SCHEMES
        .iter()
        .position(|scheme| scheme.name == name)
        .unwrap_or(0) as u8
}

const SIDEBAR_MIN_WIDTH: f32 = 220.0;
const SIDEBAR_MAX_WIDTH: f32 = 640.0;
const SIDEBAR_DIVIDER_WIDTH: f32 = 5.0;
const MENU_BAR_HEIGHT: f32 = 28.0;
const TOOLBAR_HEIGHT: f32 = 36.0;
const TAB_BAR_HEIGHT: f32 = 34.0;
const STATUS_BAR_HEIGHT: f32 = 28.0;
const TERMINAL_PANEL_PADDING: f32 = 8.0;
const TERMINAL_HEADER_AND_GAP: f32 = 0.0;
const PROFILE_ROW_HEIGHT: f32 = 36.0;
// Split-pane layout.
const PANE_GAP: f32 = 6.0;
const PANE_HEADER_HEIGHT: f32 = 26.0;
const MAX_PANES: usize = 6;

impl Default for AditApp {
    fn default() -> Self {
        let profile_store = ProfileStore::default();
        let load_result = profile_store.load_catalog();
        let (manager, groups, load_notice, load_error) = match load_result {
            Ok(catalog) if !catalog.profiles.is_empty() => {
                let count = catalog.profiles.len();
                let groups = groups_from_catalog(catalog.groups, &catalog.profiles);
                (
                    SessionManager::with_profiles(catalog.profiles),
                    groups,
                    format!(
                        "已加载 {count} 个会话配置和分组: {}",
                        profile_store.path().display()
                    ),
                    None,
                )
            }
            Ok(catalog) if !catalog.groups.is_empty() => (
                SessionManager::with_profiles(Vec::new()),
                groups_from_catalog(catalog.groups, &catalog.profiles),
                format!("已加载空分组配置: {}", profile_store.path().display()),
                None,
            ),
            Ok(_) => {
                let manager = SessionManager::with_demo_profiles();
                let groups = groups_from_profiles(manager.profiles());
                (
                    manager,
                    groups,
                    format!(
                        "使用演示会话配置，保存后写入 {}",
                        profile_store.path().display()
                    ),
                    None,
                )
            }
            Err(error) => {
                let manager = SessionManager::with_demo_profiles();
                let groups = groups_from_profiles(manager.profiles());
                (
                    manager,
                    groups,
                    format!(
                        "使用演示会话配置，保存后写入 {}",
                        profile_store.path().display()
                    ),
                    Some(format!("读取会话配置失败: {error}")),
                )
            }
        };

        Self::with_loaded_state(manager, groups, profile_store, load_notice, load_error)
    }
}

impl AditApp {
    fn with_loaded_state(
        manager: SessionManager,
        groups: BTreeSet<String>,
        profile_store: ProfileStore,
        load_notice: String,
        load_error: Option<String>,
    ) -> Self {
        let selected_profile = manager.profiles().first().map(|profile| profile.id);

        // Restore persisted preferences (theme, folded groups, window size,
        // auto-reconnect).
        let settings_store = SettingsStore::default();
        let settings = settings_store.load().unwrap_or_default();
        let dark_mode = settings.dark_mode;
        // Clamp away a bad persisted size (e.g. a 0x0 written while minimized) so
        // the window is never created invisible; the file then self-heals on the
        // next Tick because the clamped value differs from `persisted_settings`.
        let raw_window_width = settings.window_width;
        let raw_window_height = settings.window_height;
        let (window_width, window_height) = sane_window_size(raw_window_width, raw_window_height);
        let auto_reconnect = settings.auto_reconnect;
        let collapsed_groups: BTreeSet<String> = settings.collapsed_groups.into_iter().collect();
        let sidebar_width = settings
            .sidebar_width
            .clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
        let sidebar_visible = settings.sidebar_visible;
        let font_family = settings.font_family;
        let font_size = settings.font_size.clamp(MIN_FONT_SIZE as f32, MAX_FONT_SIZE as f32);
        let color_scheme = settings.color_scheme;
        let log_dir = settings.log_dir;
        let log_name_pattern = settings.log_name_pattern;
        let auto_log_on_connect = settings.auto_log_on_connect;
        let log_plaintext = settings.log_plaintext;
        let copy_on_select = settings.copy_on_select;
        let right_click_paste = settings.right_click_paste;
        let confirm_multiline_paste = settings.confirm_multiline_paste;

        let connect_timeout_secs = settings.connect_timeout_secs;
        let scrollback_lines = settings.scrollback_lines;
        adit_terminal::set_scrollback_limit(scrollback_lines as usize);
        let snippets = settings.snippets;
        let auto_check_updates = settings.auto_check_updates;
        let command_window_open = settings.command_window_open;
        let command_send_immediately = settings.command_send_immediately;

        let mut manager = manager;
        manager.set_auto_reconnect(auto_reconnect);
        manager.set_connect_timeout(u64::from(connect_timeout_secs));

        // Mirror what is on disk (raw, not clamped) so a bad size triggers one
        // corrective write, while a valid size stays untouched.
        let persisted_settings = AppSettings {
            dark_mode,
            collapsed_groups: collapsed_groups.iter().cloned().collect(),
            window_width: raw_window_width,
            window_height: raw_window_height,
            auto_reconnect,
            sidebar_width: settings.sidebar_width,
            sidebar_visible,
            font_family: font_family.clone(),
            font_size,
            color_scheme: color_scheme.clone(),
            log_dir: log_dir.clone(),
            log_name_pattern: log_name_pattern.clone(),
            auto_log_on_connect,
            log_plaintext,
            copy_on_select,
            right_click_paste,
            confirm_multiline_paste,
            connect_timeout_secs,
            scrollback_lines,
            snippets: snippets.clone(),
            auto_check_updates,
            command_window_open,
            command_send_immediately,
        };
        let effective_sidebar = if sidebar_visible { sidebar_width } else { 0.0 };

        let mut app = Self {
            manager,
            profile_store,
            credential_store: CredentialStore::default(),
            selected_profile,
            hovered_profile: None,
            dragged_profile: None,
            profile_drag_moved: false,
            profile_drag_cursor: None,
            group_drop_target: None,
            group_context_menu: None,
            editing_group: None,
            group_name_draft: String::new(),
            profile_context_menu: None,
            profile_editor: None,
            connection_dialog: None,
            groups,
            collapsed_groups,
            active_menu: None,
            profile_group: String::new(),
            profile_name: String::new(),
            profile_host: String::new(),
            profile_port: String::from("22"),
            profile_username: String::new(),
            profile_auth_method: AuthMethod::Auto,
            profile_protocol: Protocol::Ssh,
            profile_identity_file: String::new(),
            profile_startup_command: String::new(),
            profile_terminal_type: String::new(),
            connect_timeout_secs,
            scrollback_lines,
            snippets,
            snippets_open: false,
            snippet_name_draft: String::new(),
            snippet_command_draft: String::new(),
            auto_check_updates,
            password: String::new(),
            remember_connection_password: false,
            session_filter: String::new(),
            sftp_upload_path: String::new(),
            sftp_new_folder: String::new(),
            sftp_rename: None,
            sftp_rename_to: String::new(),
            sftp_delete_target: None,
            sftp_local_path_edit: String::new(),
            sftp_remote_path_edit: String::new(),
            sftp_local_cwd_seen: String::new(),
            sftp_remote_cwd_seen: String::new(),
            sftp_local_selected: BTreeSet::new(),
            sftp_remote_selected: BTreeSet::new(),
            sftp_local_sort: (SftpSortKey::Name, true),
            sftp_remote_sort: (SftpSortKey::Name, true),
            sftp_last_click: None,
            sftp_drag: None,
            sftp_drag_over: None,
            sftp_drag_cursor: None,
            tunnels_open: false,
            about_open: false,
            tunnel_kind: TunnelKind::Local,
            tunnel_bind_addr: String::from("127.0.0.1"),
            tunnel_bind_port: String::new(),
            tunnel_target_host: String::new(),
            tunnel_target_port: String::new(),
            tunnel_save: true,
            terminal_input: String::new(),
            terminal_focused: false,
            terminal_size: estimated_terminal_size(window_width, window_height, effective_sidebar),
            terminal_pointer: None,
            terminal_selection: None,
            terminal_selecting: false,
            terminal_click: None,
            terminal_context_menu: false,
            terminal_scroll_offset: 0,
            modifiers: keyboard::Modifiers::empty(),
            window_width,
            window_height,
            sidebar_width,
            sidebar_visible,
            sidebar_dragging: false,
            cursor_pos: Point::ORIGIN,
            context_menu_pos: Point::ORIGIN,
            dark_mode,
            font_family,
            font_size,
            color_scheme,
            appearance_open: false,
            update_dialog_open: false,
            update_state: UpdateState::Idle,
            options_open: false,
            log_dir,
            log_name_pattern,
            auto_log_on_connect,
            log_plaintext,
            copy_on_select,
            right_click_paste,
            confirm_multiline_paste,
            pending_paste: None,
            paste_confirm_open: false,
            mouse_button_down: false,
            mouse_report_cell: None,
            search_open: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_index: None,
            renaming_session: None,
            session_rename_draft: String::new(),
            dragged_tab: None,
            broadcast_input: false,
            command_window_open,
            command_target: CommandTarget::ActiveSession,
            command_send_immediately,
            command_history: Vec::new(),
            command_history_pos: None,
            panes: Vec::new(),
            focused_pane: 0,
            tile_mode: TileMode::Grid,
            settings_store,
            persisted_settings,
            last_error: load_error,
            notice: load_notice,
        };
        load_selected_profile(&mut app);
        app
    }
}

/// Minimum sane window dimension; anything smaller (e.g. a 0x0 saved while
/// minimized) falls back to the default so the window is never invisible.
const MIN_WINDOW_DIM: f32 = 320.0;
const DEFAULT_WINDOW_SIZE: (f32, f32) = (1360.0, 860.0);

fn sane_window_size(width: f32, height: f32) -> (f32, f32) {
    if width.is_finite() && height.is_finite() && width >= MIN_WINDOW_DIM && height >= MIN_WINDOW_DIM
    {
        (width, height)
    } else {
        DEFAULT_WINDOW_SIZE
    }
}

pub fn run() -> iced::Result {
    // Restore the saved window size (used as the restore-down size) and open
    // maximized so the window fills the screen's work area instead of a
    // centered, smaller window that leaves a gap at the top.
    let settings = SettingsStore::default().load().unwrap_or_default();
    let (width, height) = sane_window_size(settings.window_width, settings.window_height);
    // Boot: build the app and, if auto-update-check is on, fire a silent check
    // that only surfaces the dialog when a newer version exists.
    let boot = || {
        let app = AditApp::default();
        let task = if app.auto_check_updates {
            Task::perform(check_for_update(), Message::AutoUpdateChecked)
        } else {
            Task::none()
        };
        (app, task)
    };
    iced::application(boot, update, view)
        .title(app_title)
        .theme(app_theme)
        .subscription(subscription)
        .window(window::Settings {
            icon: app_icon(),
            size: iced::Size::new(width, height),
            maximized: true,
            ..window::Settings::default()
        })
        .run()
}

/// The window/taskbar icon, decoded from a raw 256x256 RGBA blob embedded in
/// the binary. Returns `None` if the blob is malformed rather than failing.
fn app_icon() -> Option<window::Icon> {
    const ICON_RGBA: &[u8] = include_bytes!("../assets/icon.rgba");
    window::icon::from_rgba(ICON_RGBA.to_vec(), 256, 256).ok()
}

fn app_title(app: &AditApp) -> String {
    format!("Adit - {}", app.manager.status_line())
}

fn app_theme(app: &AditApp) -> Theme {
    // The chrome is fully custom-styled; the base theme only drives default
    // widgets (scrollbars, checkboxes), which must match the active mode.
    if app.dark_mode {
        Theme::Dark
    } else {
        Theme::Light
    }
}

fn subscription(app: &AditApp) -> Subscription<Message> {
    let mut subs = vec![
        iced::time::every(Duration::from_millis(100)).map(|_| Message::Tick),
        event::listen_with(runtime_event),
    ];
    // Only track the global cursor while a sidebar resize is in progress, so
    // idle mouse movement never floods the app with messages.
    if app.sidebar_dragging {
        subs.push(event::listen_with(sidebar_drag_event));
    }
    // While a text selection drag is live, catch the button-up anywhere — even
    // outside the terminal panel — so the selection can't get "stuck" extending
    // after the user releases past the panel edge or over another widget.
    if app.terminal_selecting {
        subs.push(event::listen_with(terminal_release_event));
    }
    // A tab drag reorders live on hover, so it MUST be disarmed on release even
    // if the button comes up off the tab strip — otherwise merely hovering tabs
    // afterward would keep reordering them.
    if app.dragged_tab.is_some() {
        subs.push(event::listen_with(tab_release_event));
    }
    Subscription::batch(subs)
}

fn tab_release_event(
    event: event::Event,
    _status: event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        event::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
            Some(Message::TabReleased)
        }
        _ => None,
    }
}

fn terminal_release_event(
    event: event::Event,
    _status: event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        event::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
            Some(Message::EndTerminalSelection)
        }
        _ => None,
    }
}

fn sidebar_drag_event(
    event: event::Event,
    _status: event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        event::Event::Mouse(mouse::Event::CursorMoved { position }) => {
            Some(Message::SidebarDragMove(position.x))
        }
        event::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
            Some(Message::EndSidebarDrag)
        }
        _ => None,
    }
}

fn runtime_event(
    event: event::Event,
    status: event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        // Track modifier state unconditionally so Ctrl+wheel zoom works even
        // when a widget would otherwise consume the keyboard event.
        event::Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
            Some(Message::ModifiersChanged(modifiers))
        }
        event::Event::Keyboard(event) if status == event::Status::Ignored => {
            Some(Message::KeyboardInput(event))
        }
        event::Event::Window(window::Event::Opened { size, .. })
        | event::Event::Window(window::Event::Resized(size)) => Some(Message::WindowResized {
            width: size.width,
            height: size.height,
        }),
        event::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            if status == event::Status::Ignored =>
        {
            Some(Message::CancelProfileDrag)
        }
        // Files dragged from the OS file manager onto the window.
        event::Event::Window(window::Event::FileDropped(path)) => {
            Some(Message::SftpFileDropped(path))
        }
        _ => None,
    }
}

fn update(app: &mut AditApp, message: Message) -> Task<Message> {
    match message {
        Message::Tick => {
            app.manager.poll_events();
            auto_log_connected_sessions(app);
            clamp_terminal_scroll(app);
            sync_sftp_state(app);
            // Reconcile split panes with the live session set (closed sessions,
            // an externally-activated session); refit only if the count changed.
            let panes_before = app.panes.len();
            sync_panes(app);
            if app.panes.len() != panes_before {
                sync_terminal_size(app);
            }
            persist_settings_if_changed(app);
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
            app.dark_mode = !app.dark_mode;
            app.notice = if app.dark_mode {
                String::from("已切换到深色主题")
            } else {
                String::from("已切换到浅色主题")
            };
        }
        Message::OpenAppearance => {
            app.appearance_open = true;
            app.active_menu = None;
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
        Message::OpenOptions => {
            app.options_open = true;
            app.active_menu = None;
        }
        Message::CloseOptions => {
            app.options_open = false;
        }
        Message::LogDirChanged(value) => {
            app.log_dir = value;
        }
        Message::LogNamePatternChanged(value) => {
            app.log_name_pattern = value;
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
            open_folder(app, adit_storage::config_dir());
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
            run_menu_command(app, command);
            app.active_menu = None;
            sync_terminal_size(app);
        }
        Message::SelectProfile(profile_id) => {
            select_profile(app, profile_id);
            app.profile_context_menu = None;
            app.group_context_menu = None;
            close_profile_editor_if_other(app, profile_id);
        }
        Message::ProfilePressed(profile_id) => {
            select_profile(app, profile_id);
            app.dragged_profile = Some(profile_id);
            app.profile_drag_moved = false;
            // Seed the floating ghost at the press point (cursor_pos is window
            // absolute; the sidebar starts just below the menu bar + toolbar).
            app.profile_drag_cursor = Some(Point::new(
                app.cursor_pos.x,
                app.cursor_pos.y - MENU_BAR_HEIGHT - TOOLBAR_HEIGHT,
            ));
            app.group_drop_target = None;
            app.profile_context_menu = None;
            app.group_context_menu = None;
            close_profile_editor_if_other(app, profile_id);
        }
        Message::ProfileDoubleClicked(profile_id) => {
            select_profile(app, profile_id);
            app.dragged_profile = None;
            app.group_drop_target = None;
            app.profile_context_menu = None;
            app.group_context_menu = None;
            app.profile_editor = None;
            // Double-click connects immediately, like SecureCRT/Xshell — only
            // fall back to the dialog when a password is genuinely required.
            connect_profile(app);
        }
        Message::ProfileHovered(profile_id) => {
            app.hovered_profile = Some(profile_id);
            // iced's mouse_area fires on_enter (this) — not on_move — on the frame
            // the cursor crosses into a new row, so the live reorder has to happen
            // here to reliably trigger on every crossing (on_move alone misses a
            // fast drag). Direction is derived from the current order.
            live_reorder_profile(app, profile_id);
        }
        Message::ProfileHoverExited(profile_id) => {
            if app.hovered_profile == Some(profile_id) {
                app.hovered_profile = None;
            }
        }
        Message::ProfileDragOver(profile_id, _position) => {
            app.hovered_profile = Some(profile_id);
            // Continued movement within a row also reorders (redundant with the
            // on_enter path, but keeps things responsive on a slow drag).
            live_reorder_profile(app, profile_id);
        }
        Message::ProfileDropped(_profile_id) => {
            // Live reorder already positioned the row; just finalize + persist.
            finish_profile_drag(app);
        }
        Message::ProfileDragOverGroup(group) => {
            if app.dragged_profile.is_some() {
                app.group_drop_target = Some(group);
            }
        }
        Message::ProfileDroppedOnGroup(group) => {
            drop_profile_on_group(app, group);
        }
        Message::ProfileGroupHoverExited(group) => {
            if app.dragged_profile.is_none()
                && app.group_drop_target.as_deref() == Some(group.as_str())
            {
                app.group_drop_target = None;
            }
        }
        Message::CancelProfileDrag => {
            finish_profile_drag(app);
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
            app.group_context_menu = Some(group);
            app.profile_context_menu = None;
            app.profile_editor = None;
            app.terminal_context_menu = false;
        }
        Message::HideGroupContextMenu => {
            app.group_context_menu = None;
        }
        Message::RenameGroupFromContext(group) => {
            app.group_context_menu = None;
            app.editing_group = Some(group.clone());
            app.group_name_draft = group;
        }
        Message::NewProfileInGroup(group) => {
            app.group_context_menu = None;
            app.profile_group = group;
            new_profile_draft(app);
        }
        Message::DeleteGroupFromContext(group) => {
            delete_empty_group(app, group);
        }
        Message::GroupNameDraftChanged(value) => {
            app.group_name_draft = value;
        }
        Message::SaveGroupRename => {
            save_group_rename(app);
        }
        Message::CancelGroupRename => {
            app.editing_group = None;
            app.group_name_draft.clear();
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
        }
        Message::HideProfileContextMenu => {
            app.profile_context_menu = None;
        }
        Message::SidebarCursorMoved(point) => {
            // `point` is sidebar-relative; the context-menu anchor wants it in
            // window-absolute coordinates.
            app.cursor_pos = Point::new(point.x, point.y + MENU_BAR_HEIGHT + TOOLBAR_HEIGHT);
            if app.dragged_profile.is_some() {
                app.profile_drag_cursor = Some(point);
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
                // Copy the source's saved password (kept in the OS vault under the
                // profile id) to the clone so its auth still works.
                if let Ok(Some(password)) = app.credential_store.load_profile_password(profile_id) {
                    let _ = app.credential_store.save_profile_password(new_id, &password);
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
        Message::OpenSftp => {
            if let Err(error) = app.manager.open_sftp_for_active() {
                app.last_error = Some(format!("打开 SFTP 失败: {error}"));
            } else {
                app.last_error = None;
            }
        }
        Message::CloseSftp => {
            app.manager.close_sftp();
            app.sftp_rename = None;
            app.sftp_delete_target = None;
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
        Message::SftpNavigate(name) => app.manager.sftp_navigate(&name),
        Message::SftpUp => app.manager.sftp_up(),
        Message::SftpRefresh => app.manager.sftp_refresh(),
        Message::SftpLocalNavigate(name) => app.manager.sftp_local_navigate(&name),
        Message::SftpLocalUp => app.manager.sftp_local_up(),
        Message::SftpLocalRefresh => app.manager.sftp_local_refresh(),
        Message::SftpUploadLocal(name) => app.manager.sftp_upload_local(&name),
        Message::SftpDownload(name) => app.manager.sftp_download(&name),
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
            if !app.manager.sftp_is_open() {
                app.notice = String::from("拖拽上传：请先打开 SFTP 面板");
            } else if path.is_dir() {
                app.last_error = Some(String::from("暂不支持上传文件夹，请拖入单个文件"));
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
        Message::SftpDragEnter(pane) => app.sftp_drag_over = Some(pane),
        Message::SftpDragMove(pane, position) => {
            if app.sftp_drag.is_some() {
                app.sftp_drag_over = Some(pane);
                app.sftp_drag_cursor = Some(position);
            }
        }
        Message::ToggleProfileGroup(group) => {
            if !app.collapsed_groups.remove(&group) {
                app.collapsed_groups.insert(group);
            }
            app.profile_context_menu = None;
            app.group_context_menu = None;
            app.profile_editor = None;
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
        Message::ProfileProtocolChanged(protocol) => {
            app.terminal_focused = false;
            // Nudge the port to a sensible default when moving to/from RDP.
            let port = app.profile_port.trim();
            if protocol == Protocol::Rdp && (port.is_empty() || port == "22") {
                app.profile_port = String::from("3389");
            } else if protocol == Protocol::Ssh && port == "3389" {
                app.profile_port = String::from("22");
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
        Message::MoveSelectedProfile(direction) => {
            move_selected_profile(app, direction);
        }
        Message::SortProfiles(key) => {
            sort_profiles(app, key);
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
        Message::KeyboardInput(event) => {
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
            // Minimizing reports a 0x0 size on Windows; ignore it so we never
            // persist (and later restore) an invisible window.
            if width >= MIN_WINDOW_DIM && height >= MIN_WINDOW_DIM {
                app.window_width = width;
                app.window_height = height;
                sync_terminal_size(app);
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
        Message::FocusTerminal => {
            if !app.terminal_focused {
                app.notice = String::from("终端已聚焦，键盘输入会发送到当前会话");
            }
            app.terminal_focused = true;
            app.terminal_context_menu = false;
        }
        Message::SplitPane => {
            split_pane(app);
        }
        Message::ClosePane(index) => {
            close_pane(app, index);
        }
        Message::FocusPane(index) => {
            focus_pane(app, index);
        }
        Message::PaneMousePressed(index) => {
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
                if let Some(selection) = &mut app.terminal_selection {
                    selection.end = terminal_point;
                }
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
                if let Some(selection) = &mut app.terminal_selection {
                    selection.end = terminal_point;
                }
            }
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
            open_connection_dialog(app);
        }
        Message::RetryActiveSession => {
            retry_active_session(app);
        }
        Message::ActivateSession(session_id) => {
            activate_session(app, session_id);
        }
        Message::TabPressed(session_id) => {
            // Clicking a tab activates it and arms a possible drag-reorder.
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
            app.manager.close(session_id);
            app.terminal_scroll_offset = 0;
            app.terminal_selection = None;
            app.terminal_context_menu = false;
            app.notice = String::from("标签已关闭");
        }
        Message::RenameSessionPrompt(session_id) => {
            let current = app
                .manager
                .session_summary(session_id)
                .map(|summary| summary.title)
                .unwrap_or_default();
            app.session_rename_draft = current;
            app.renaming_session = Some(session_id);
            app.terminal_focused = false;
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
        Message::OpenSearch => {
            app.search_open = true;
            app.terminal_focused = false;
            recompute_search(app);
            return focus_search_input();
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
        Message::ToggleAutoCheckUpdates(enabled) => {
            app.auto_check_updates = enabled;
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

/// Kick off an update check: show the dialog in the "checking" state and query
/// GitHub in the background.
fn begin_update_check(app: &mut AditApp) -> Task<Message> {
    app.active_menu = None;
    app.update_dialog_open = true;
    app.update_state = UpdateState::Checking;
    Task::perform(check_for_update(), Message::UpdateChecked)
}

/// Open a URL in the default browser (best-effort).
fn open_url(app: &mut AditApp, url: &str) {
    let result = if cfg!(target_os = "windows") {
        no_window(std::process::Command::new("cmd").args(["/C", "start", "", url])).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(error) = result {
        app.last_error = Some(format!("打开链接失败: {error}"));
    }
}

/// Suppress the console window when spawning a console tool from the GUI app.
fn no_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

const UPDATE_REPO: &str = "weironz/adit";

/// Check GitHub for a newer release. `Ok(None)` = already up to date.
async fn check_for_update() -> Result<Option<UpdateInfo>, String> {
    tokio::task::spawn_blocking(check_for_update_blocking)
        .await
        .map_err(|error| format!("更新检查任务失败: {error}"))?
}

fn check_for_update_blocking() -> Result<Option<UpdateInfo>, String> {
    let url = format!("https://api.github.com/repos/{UPDATE_REPO}/releases/latest");
    let output = no_window(std::process::Command::new("curl").args([
        "-sSL",
        "--max-time",
        "25",
        "-H",
        "User-Agent: adit-updater",
        "-H",
        "Accept: application/vnd.github+json",
        &url,
    ]))
    .output()
    .map_err(|error| format!("无法运行 curl（检查更新需要系统自带的 curl）: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("检查更新失败: {}", stderr.trim()));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("解析发布信息失败: {error}"))?;

    let tag = json["tag_name"]
        .as_str()
        .ok_or("发布信息缺少 tag_name")?
        .to_string();
    let notes_url = json["html_url"].as_str().unwrap_or_default().to_string();

    let current = env!("CARGO_PKG_VERSION");
    if !version_is_newer(&tag, current) {
        return Ok(None);
    }

    // Pick the Windows installer asset (the .exe).
    let asset = json["assets"].as_array().and_then(|assets| {
        assets
            .iter()
            .find(|asset| asset["name"].as_str().is_some_and(|n| n.ends_with(".exe")))
    });
    let (installer_url, installer_name) = match asset {
        Some(asset) => (
            asset["browser_download_url"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            asset["name"].as_str().unwrap_or_default().to_string(),
        ),
        None => (String::new(), String::new()),
    };

    Ok(Some(UpdateInfo {
        tag,
        installer_url,
        installer_name,
        notes_url,
    }))
}

/// Build the command to run the downloaded installer as a silent background
/// update: no wizard, installed in place over the current location, then the
/// installer relaunches Adit. A UAC prompt still appears for an all-users
/// (Program Files) install.
fn launch_silent_update(installer_path: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(installer_path);
    cmd.args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"]);

    // Update in place at the current install directory + scope, so a background
    // update never creates a second copy elsewhere.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cmd.arg(format!("/DIR={}", dir.display()));
            let in_program_files = dir
                .to_string_lossy()
                .to_lowercase()
                .contains("program files");
            cmd.arg(if in_program_files {
                "/ALLUSERS"
            } else {
                "/CURRENTUSER"
            });
        }
    }
    cmd
}

/// Download the installer to a temp folder; returns the saved path.
async fn download_installer(url: String, name: String) -> Result<String, String> {
    if url.is_empty() {
        return Err(String::from("该版本没有可下载的 Windows 安装包"));
    }
    tokio::task::spawn_blocking(move || download_installer_blocking(&url, &name))
        .await
        .map_err(|error| format!("下载任务失败: {error}"))?
}

fn download_installer_blocking(url: &str, name: &str) -> Result<String, String> {
    let dir = std::env::temp_dir().join("adit-update");
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建下载目录失败: {error}"))?;
    let safe_name = if name.is_empty() { "adit-installer.exe" } else { name };
    let dest = dir.join(safe_name);

    let output = no_window(std::process::Command::new("curl").args([
        "-sSL",
        "--max-time",
        "600",
        "-H",
        "User-Agent: adit-updater",
        "-o",
        &dest.to_string_lossy(),
        url,
    ]))
    .output()
    .map_err(|error| format!("无法运行 curl: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("下载安装包失败: {}", stderr.trim()));
    }

    match std::fs::metadata(&dest) {
        Ok(meta) if meta.len() >= 200_000 => Ok(dest.to_string_lossy().to_string()),
        Ok(_) => Err(String::from("下载的安装包不完整，请重试")),
        Err(error) => Err(format!("找不到下载的安装包: {error}")),
    }
}

/// Compare a `vX.Y.Z` (or `X.Y.Z`) tag against the current version.
fn version_is_newer(latest: &str, current: &str) -> bool {
    parse_semver(latest) > parse_semver(current)
}

fn parse_semver(value: &str) -> (u32, u32, u32) {
    let mut parts = value
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|part| part.trim().parse::<u32>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

fn run_menu_command(app: &mut AditApp, command: MenuCommand) {
    match command {
        MenuCommand::NewProfile => new_profile_draft(app),
        MenuCommand::NewGroup => new_group_draft(app),
        MenuCommand::SaveProfile => save_profile(app),
        MenuCommand::DeleteProfile => delete_selected_profile(app),
        MenuCommand::Connect => open_connection_dialog(app),
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
        MenuCommand::Appearance => app.appearance_open = true,
        MenuCommand::Options => app.options_open = true,
        MenuCommand::ImportSshConfig => import_ssh_config(app),
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

fn select_profile(app: &mut AditApp, profile_id: ProfileId) {
    app.terminal_focused = false;
    app.selected_profile = Some(profile_id);
    load_selected_profile(app);
    app.last_error = None;
}

fn close_profile_editor_if_other(app: &mut AditApp, profile_id: ProfileId) {
    if app
        .profile_editor
        .is_some_and(|editing| editing != profile_id)
    {
        app.profile_editor = None;
    }
}

/// Live-reorder the dragged profile so it sits next to `target`, choosing the
/// side from the current order (dragged is above target ⇒ drop after, below ⇒
/// drop before). Mirrors the session-tab drag so the held row slides under the
/// cursor. A no-op when nothing is being dragged or the cursor is over the
/// dragged row itself, which keeps the drag stable once it lands.
fn live_reorder_profile(app: &mut AditApp, target_id: ProfileId) {
    let Some(source_id) = app.dragged_profile else {
        return;
    };
    if source_id == target_id {
        return;
    }
    let index_of = |app: &AditApp, id: ProfileId| {
        app.manager.profiles().iter().position(|p| p.id == id)
    };
    let (Some(source_index), Some(target_index)) =
        (index_of(app, source_id), index_of(app, target_id))
    else {
        return;
    };
    let position = if source_index < target_index {
        ProfileDropPosition::After
    } else {
        ProfileDropPosition::Before
    };
    if app
        .manager
        .reorder_profile(source_id, target_id, position)
        .is_ok()
    {
        app.profile_drag_moved = true;
        app.selected_profile = Some(source_id);
        app.group_drop_target = None;
    }
}

fn finish_profile_drag(app: &mut AditApp) {
    app.profile_drag_cursor = None;
    if app.dragged_profile.is_none() {
        app.group_drop_target = None;
        app.profile_drag_moved = false;
        return;
    }

    // Released over a group header (e.g. an empty or different group) with no
    // row under the pointer: move into that group. `drop_profile_on_group`
    // takes `dragged_profile` and persists itself.
    if let Some(group) = app.group_drop_target.clone() {
        drop_profile_on_group(app, group);
    } else {
        let source_id = app.dragged_profile.take();
        // A live reorder already rearranged the rows in memory; persist it once
        // on release (a plain click never sets `profile_drag_moved`, so it won't
        // rewrite profiles.json).
        if app.profile_drag_moved {
            app.selected_profile = source_id;
            load_selected_profile(app);
            if persist_profiles(app) {
                app.notice = String::from("会话排序已更新");
            }
        }
    }

    app.group_drop_target = None;
    app.profile_drag_moved = false;
}

fn drop_profile_on_group(app: &mut AditApp, group: String) {
    let Some(source_id) = app.dragged_profile.take() else {
        app.group_drop_target = None;
        return;
    };

    app.group_drop_target = None;

    match app.manager.move_profile_to_group(source_id, group.clone()) {
        Ok(()) => {
            app.groups.insert(group.clone());
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

fn load_selected_profile(app: &mut AditApp) {
    let profile = app
        .selected_profile
        .and_then(|profile_id| app.manager.profile(profile_id).cloned());

    if let Some(profile) = profile {
        app.profile_group = profile.group;
        app.groups.insert(app.profile_group.clone());
        app.profile_name = profile.name;
        app.profile_host = profile.host;
        app.profile_port = profile.port.to_string();
        app.profile_username = profile.username;
        app.profile_auth_method = profile.auth_method;
        app.profile_identity_file = profile.identity_file;
        app.profile_protocol = profile.protocol;
        app.profile_startup_command = profile.startup_command;
        app.profile_terminal_type = profile.terminal_type;
    }
}

fn new_profile_draft(app: &mut AditApp) {
    let name = next_profile_name(app);
    let group = active_profile_group(app);
    match app.manager.create_profile(
        group.clone(),
        name,
        "127.0.0.1",
        22,
        "root",
        AuthMethod::Auto,
        "",
    ) {
        Ok(profile_id) => {
            app.selected_profile = Some(profile_id);
            app.profile_editor = Some(profile_id);
            app.groups.insert(group.clone());
            app.collapsed_groups.remove(&group);
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

fn new_group_draft(app: &mut AditApp) {
    let group = next_group_name(app);
    app.groups.insert(group.clone());
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

fn save_group_rename(app: &mut AditApp) {
    let Some(old_group) = app.editing_group.clone() else {
        return;
    };
    let new_group = app.group_name_draft.trim().to_string();

    if new_group.is_empty() {
        app.last_error = Some(String::from("分组名称不能为空"));
        return;
    }

    if old_group != new_group && app.groups.contains(&new_group) {
        app.last_error = Some(format!("分组已存在: {new_group}"));
        return;
    }

    match app.manager.rename_group(&old_group, new_group.clone()) {
        Ok(()) => {
            app.groups.remove(&old_group);
            app.groups.insert(new_group.clone());

            if app.collapsed_groups.remove(&old_group) {
                app.collapsed_groups.insert(new_group.clone());
            }

            if app.profile_group == old_group {
                app.profile_group = new_group.clone();
            }

            app.editing_group = None;
            app.group_name_draft.clear();
            app.last_error = None;

            if persist_profiles(app) {
                app.notice = format!("分组已重命名: {old_group} -> {new_group}");
            }
        }
        Err(error) => {
            app.last_error = Some(error.to_string());
        }
    }
}

fn delete_empty_group(app: &mut AditApp, group: String) {
    app.group_context_menu = None;
    app.editing_group = None;
    app.group_name_draft.clear();

    if app
        .manager
        .profiles()
        .iter()
        .any(|profile| profile.group == group)
    {
        app.last_error = Some(String::from("分组非空，请先移动或删除其中的会话"));
        return;
    }

    if app.groups.remove(&group) {
        app.collapsed_groups.remove(&group);
        if app.profile_group == group {
            app.profile_group = String::from("Default");
        }
        if persist_profiles(app) {
            app.notice = format!("空分组已删除: {group}");
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

fn next_group_name(app: &AditApp) -> String {
    let mut index = app.groups.len() + 1;
    loop {
        let name = format!("group-{index}");
        if !app.groups.contains(&name) {
            return name;
        }
        index += 1;
    }
}

fn active_profile_group(app: &AditApp) -> String {
    let group = app.profile_group.trim();
    if group.is_empty() {
        String::from("Default")
    } else {
        group.to_string()
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
                app.manager.set_profile_startup_command(
                    profile_id,
                    app.profile_startup_command.clone(),
                );
                app.manager
                    .set_profile_terminal_type(profile_id, app.profile_terminal_type.clone());
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

fn delete_selected_profile(app: &mut AditApp) {
    let Some(profile_id) = app.selected_profile else {
        app.last_error = Some(String::from("请选择要删除的会话配置"));
        return;
    };

    match app.manager.delete_profile(profile_id) {
        Ok(()) => {
            app.profile_context_menu = None;
            app.profile_editor = None;
            app.selected_profile = app.manager.profiles().first().map(|profile| profile.id);
            app.last_error = None;
            let credential_cleanup = app
                .credential_store
                .delete_profile_password(profile_id)
                .err();
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

/// Import hosts from `~/.ssh/config` into the profile list (group "Imported"),
/// skipping any whose name already exists.
fn import_ssh_config(app: &mut AditApp) {
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
        app.groups.insert(group.to_string());
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

fn persist_profiles(app: &mut AditApp) -> bool {
    let catalog = ProfileCatalog::new(
        app.groups.iter().cloned().collect(),
        app.manager.profiles().to_vec(),
    );

    match app.profile_store.save_catalog(&catalog) {
        Ok(()) => true,
        Err(error) => {
            app.last_error = Some(format!("保存会话配置失败: {error}"));
            false
        }
    }
}

fn groups_from_catalog(groups: Vec<String>, profiles: &[ConnectionProfile]) -> BTreeSet<String> {
    let mut result = groups.into_iter().collect::<BTreeSet<_>>();
    result.extend(groups_from_profiles(profiles));
    if result.is_empty() {
        result.insert(String::from("Default"));
    }
    result
}

fn groups_from_profiles(profiles: &[ConnectionProfile]) -> BTreeSet<String> {
    profiles
        .iter()
        .map(|profile| profile.group.trim())
        .filter(|group| !group.is_empty())
        .map(ToOwned::to_owned)
        .collect()
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

/// Connect to a profile directly (used by double-click). Uses a stored password
/// when present; for key/agent/auto auth it connects with no password; only
/// password auth without a stored secret falls back to the connection dialog.
fn connect_profile(app: &mut AditApp) {
    let Some(profile_id) = save_profile_from_form(app, false) else {
        return;
    };
    let Some(profile) = app.manager.profile(profile_id).cloned() else {
        app.last_error = Some(String::from("请选择要连接的会话配置"));
        return;
    };

    // RDP is graphical — it opens in the system client, not a terminal tab.
    if profile.protocol == Protocol::Rdp {
        match app.manager.launch_rdp(profile_id) {
            Ok(endpoint) => {
                app.last_error = None;
                app.notice = format!("已调起远程桌面 (mstsc): {endpoint}");
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

    match app.manager.open_live_ssh_session(profile_id, password) {
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

fn open_connection_dialog(app: &mut AditApp) {
    let Some(profile_id) = save_profile_from_form(app, false) else {
        return;
    };
    let Some(profile) = app.manager.profile(profile_id).cloned() else {
        app.last_error = Some(String::from("请选择要连接的会话配置"));
        return;
    };

    // Only SSH uses the password dialog; other protocols connect directly (or
    // launch externally, for RDP).
    if profile.protocol != Protocol::Ssh {
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
            app.notice = String::from("已载入系统凭据库中的密码");
        }
        Ok(None) => {
            app.password.clear();
            app.remember_connection_password = false;
            app.last_error = None;
            app.notice = String::from("请输入本次连接的密码或 passphrase");
        }
        Err(error) => {
            app.password.clear();
            app.remember_connection_password = false;
            app.last_error = Some(format!("读取系统凭据失败: {error}"));
            app.notice = String::from("请输入本次连接的密码或 passphrase");
        }
    }

    app.terminal_focused = false;
    app.terminal_context_menu = false;
    app.profile_context_menu = None;
    app.group_context_menu = None;
}

fn confirm_connection(app: &mut AditApp) {
    let Some(dialog) = app.connection_dialog.clone() else {
        open_connection_dialog(app);
        return;
    };

    let credential_warning = sync_connection_password(app, dialog.profile_id).err();

    match app
        .manager
        .open_live_ssh_session(dialog.profile_id, app.password.clone())
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
            app.manager.start_profile_tunnels(dialog.profile_id);
            app.last_error = credential_warning
                .as_ref()
                .map(|error| format!("保存系统凭据失败: {error}"));
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

fn retry_active_session(app: &mut AditApp) {
    let Some(summary) = app.manager.active_session_summary() else {
        app.last_error = Some(String::from("没有可重连的活动标签"));
        return;
    };

    if !matches!(
        summary.status,
        SessionStatus::Error | SessionStatus::Disconnected
    ) {
        app.notice = String::from("当前会话仍在连接或已连接，无需重连");
        return;
    }

    select_profile(app, summary.profile_id);
    app.manager.close(summary.id);
    app.terminal_scroll_offset = 0;
    app.terminal_selection = None;
    app.terminal_context_menu = false;
    app.notice = format!("准备重连: {}", summary.endpoint);
    open_connection_dialog(app);
}

fn sync_connection_password(
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

/// Send the command-window line to its target (active session or broadcast),
/// append a carriage return, remember it in history, and keep focus in the box.
fn send_terminal_input(app: &mut AditApp) -> Task<Message> {
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
fn command_input_delta(old: &str, new: &str) -> Option<Vec<u8>> {
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
fn send_command_bytes(app: &mut AditApp, bytes: Vec<u8>) {
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
fn command_history_step(app: &mut AditApp, delta: i32) {
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

fn command_input_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("command-window-input")
}

fn focus_command_input() -> Task<Message> {
    iced::advanced::widget::operate(iced::advanced::widget::operation::focusable::focus(
        command_input_id(),
    ))
}

fn send_terminal_bytes(app: &mut AditApp, bytes: Vec<u8>) {
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
fn mouse_reporting_active(app: &AditApp) -> bool {
    app.manager.active_mouse_mode() != MouseMode::Off
}

/// Encode a mouse event as an xterm report. `button`: 0=left, 1=middle,
/// 2=right, 3=none/release, 64=wheel-up, 65=wheel-down. `col`/`row` are 0-based
/// cells; `press` is the button-down (or wheel) edge; `motion` marks a drag.
fn encode_mouse_event(sgr: bool, button: u8, col: usize, row: usize, press: bool, motion: bool) -> Vec<u8> {
    let mut cb = u32::from(button);
    if motion {
        cb += 32;
    }
    let cx = col + 1;
    let cy = row + 1;

    if sgr {
        let terminator = if press { 'M' } else { 'm' };
        format!("\x1b[<{cb};{cx};{cy}{terminator}").into_bytes()
    } else {
        // Legacy X10: ESC [ M  (Cb+32)  (Cx+32)  (Cy+32), coords capped at 223.
        let cb_byte = (cb + 32).min(255) as u8;
        let cx_byte = (32 + cx.min(223)) as u8;
        let cy_byte = (32 + cy.min(223)) as u8;
        vec![0x1b, b'[', b'M', cb_byte, cx_byte, cy_byte]
    }
}

/// Encode + send a mouse report to the active session at the current pointer
/// cell (raw send — not broadcast, does not touch the selection).
fn send_mouse_report(app: &mut AditApp, button: u8, press: bool, motion: bool) {
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
fn maybe_report_mouse_motion(app: &mut AditApp) -> bool {
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

fn is_escape_key(event: &keyboard::Event) -> bool {
    matches!(
        event,
        keyboard::Event::KeyPressed {
            key: Key::Named(Named::Escape),
            ..
        }
    )
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

/// Send pasted text to the terminal, wrapping it in the bracketed-paste markers
/// (`ESC[200~` … `ESC[201~`) when the app has enabled DEC mode 2004.
fn perform_paste(app: &mut AditApp, contents: &str, bracketed: bool) {
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
    app.notice = String::from("已粘贴到当前终端");
}

fn search_input_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("terminal-search")
}

/// A Task that moves keyboard focus to the search input.
fn focus_search_input() -> Task<Message> {
    iced::advanced::widget::operate(iced::advanced::widget::operation::focusable::focus(
        search_input_id(),
    ))
}

/// Recompute scrollback-search matches over the active session's full buffer
/// (ASCII case-insensitive), then jump to the last (most recent) match.
fn recompute_search(app: &mut AditApp) {
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
fn step_search(app: &mut AditApp, delta: i32) {
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
fn scroll_to_current_match(app: &mut AditApp) {
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
fn search_highlights_for(app: &AditApp, snapshot: &TerminalSnapshot) -> Vec<Vec<(usize, usize, bool)>> {
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
        mouse::ScrollDelta::Pixels { y, .. } => y / cell_height(),
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

/// Handle a left-press on the terminal grid, deciding — from how quickly it
/// follows the previous press on the same cell — whether it starts a drag
/// selection (single), selects a word (double), or selects a line (triple).
fn begin_terminal_click(app: &mut AditApp) {
    let point = app
        .terminal_pointer
        .unwrap_or(TerminalPoint { row: 0, col: 0 });
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

/// Select the whole word under `point` (double-click).
fn select_word_at(app: &mut AditApp, point: TerminalPoint) {
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
fn select_line_at(app: &mut AditApp, point: TerminalPoint) {
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

fn snapshot_line_text(snapshot: &TerminalSnapshot, row: usize) -> String {
    snapshot.lines.get(row).map(raw_line_text).unwrap_or_default()
}

/// Word span `[start, end)` (char indices) around `col` for double-click select.
/// "Word" chars are alphanumerics plus the punctuation that commonly appears
/// mid-token in paths, URLs, and hostnames, so `/usr/local/bin` stays one word.
fn word_bounds(line: &str, col: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    if col >= chars.len() {
        return None;
    }
    let is_word =
        |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~' | ':' | '@' | '+');
    if !is_word(chars[col]) {
        // On whitespace or a separator: grab just that single cell.
        return Some((col, col + 1));
    }
    let mut start = col;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col + 1;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    Some((start, end))
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

fn toggle_active_logging(app: &mut AditApp) {
    let Some(summary) = app.manager.active_session_summary() else {
        app.last_error = Some(String::from("没有活动会话"));
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
            Err(error) => app.last_error = Some(format!("开启会话日志失败: {error}")),
        }
    }
}

/// Default session-log filename pattern (SecureCRT-style tokens).
const DEFAULT_LOG_PATTERN: &str = "%N_%Y-%M-%D_%h-%m-%s.log";

/// The effective log folder: the user's override, else the default under the
/// configuration folder.
fn effective_log_dir(app: &AditApp) -> std::path::PathBuf {
    if app.log_dir.trim().is_empty() {
        adit_storage::default_log_dir()
    } else {
        std::path::PathBuf::from(app.log_dir.trim())
    }
}

fn effective_log_pattern(app: &AditApp) -> String {
    if app.log_name_pattern.trim().is_empty() {
        DEFAULT_LOG_PATTERN.to_string()
    } else {
        app.log_name_pattern.clone()
    }
}

/// Current local time broken into (year, month, day, hour, minute, second).
fn now_local_parts() -> (i32, u8, u8, u8, u8, u8) {
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
fn render_log_name(pattern: &str, session_name: &str, endpoint: &str) -> String {
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
fn open_folder(app: &mut AditApp, dir: std::path::PathBuf) {
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
        Err(error) => app.last_error = Some(format!("打开目录失败: {error}")),
    }
}

/// Start logging any freshly-connected sessions when auto-log is enabled.
fn auto_log_connected_sessions(app: &mut AditApp) {
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
            app.last_error = Some(format!("自动日志开启失败: {error}"));
        }
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

/// Pixel size of the whole terminal region (the grid the panes share), i.e. the
/// workspace minus the sidebar, top chrome, tab bar, and status bar. Pane
/// padding/headers are *not* subtracted here — that happens per pane.
fn terminal_region_area(width: f32, height: f32, sidebar_width: f32) -> (f32, f32) {
    let region_width = (width - sidebar_width).max(0.0);
    let region_height =
        (height - MENU_BAR_HEIGHT - TOOLBAR_HEIGHT - TAB_BAR_HEIGHT - STATUS_BAR_HEIGHT).max(0.0);
    (region_width, region_height)
}

/// Cols/rows that fit in a single pane's *inner* pixel area (after its own
/// padding + header have already been removed by the caller).
fn terminal_size_for_area(inner_width: f32, inner_height: f32) -> TerminalSize {
    let cols = (inner_width / cell_width()).floor().clamp(20.0, 220.0) as u16;
    let rows = (inner_height / cell_height()).floor().clamp(6.0, 80.0) as u16;
    TerminalSize::new(cols, rows)
}

/// How many columns × rows of panes a given pane count tiles into.
/// How split panes are arranged (SecureCRT-style tiling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TileMode {
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
enum CommandTarget {
    /// Only the active session/tab.
    #[default]
    ActiveSession,
    /// Every connected session at once (broadcast).
    AllSessions,
}

impl CommandTarget {
    fn label(self) -> &'static str {
        match self {
            CommandTarget::ActiveSession => "当前会话",
            CommandTarget::AllSessions => "所有会话",
        }
    }

    fn toggled(self) -> Self {
        match self {
            CommandTarget::ActiveSession => CommandTarget::AllSessions,
            CommandTarget::AllSessions => CommandTarget::ActiveSession,
        }
    }
}

fn pane_grid_dims(count: usize, mode: TileMode) -> (usize, usize) {
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
struct PaneLayout {
    cols: usize,
    pane_w: f32,
    pane_h: f32,
    origin_x: f32,
    origin_y: f32,
    header: f32,
}

fn pane_layout(app: &AditApp) -> PaneLayout {
    let effective_sidebar = if app.sidebar_visible {
        app.sidebar_width + SIDEBAR_DIVIDER_WIDTH
    } else {
        0.0
    };
    let (region_w, region_h) =
        terminal_region_area(app.window_width, app.window_height, effective_sidebar);

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
        origin_y: MENU_BAR_HEIGHT + TOOLBAR_HEIGHT + TAB_BAR_HEIGHT,
        header,
    }
}

impl PaneLayout {
    /// Cols/rows that fit one pane in this layout.
    fn pane_terminal_size(&self) -> TerminalSize {
        let inner_w = self.pane_w - TERMINAL_PANEL_PADDING * 2.0;
        let inner_h = self.pane_h - self.header - TERMINAL_PANEL_PADDING * 2.0;
        terminal_size_for_area(inner_w, inner_h)
    }

    /// Screen-space top-left of a pane's terminal *body* (below its header).
    fn pane_body_origin(&self, index: usize) -> Point {
        let gc = index % self.cols;
        let gr = index / self.cols;
        Point::new(
            self.origin_x + gc as f32 * (self.pane_w + PANE_GAP),
            self.origin_y + gr as f32 * (self.pane_h + PANE_GAP) + self.header,
        )
    }
}

/// Single-pane / no-split terminal size, for the common path and the status bar.
fn estimated_terminal_size(width: f32, height: f32, sidebar_width: f32) -> TerminalSize {
    let (region_w, region_h) = terminal_region_area(width, height, sidebar_width);
    terminal_size_for_area(
        region_w - TERMINAL_PANEL_PADDING * 2.0,
        region_h - TERMINAL_PANEL_PADDING * 2.0,
    )
}

/// Adjust the terminal font size by `delta` px, clamped to the sane range, and
/// re-fit the grid. Shared by the appearance dialog's +/- and Ctrl+wheel zoom.
fn step_font_size(app: &mut AditApp, delta: i32) {
    let next = (app.font_size as i32 + delta).clamp(MIN_FONT_SIZE as i32, MAX_FONT_SIZE as i32);
    app.font_size = next as f32;
    sync_terminal_size(app);
}

fn sync_terminal_size(app: &mut AditApp) {
    let layout = pane_layout(app);
    let target = layout.pane_terminal_size();

    // Skip the common no-change case so a window drag does not spam resizes.
    // Pane add/close changes the pane count → the per-pane target changes, so
    // this still fits panes on split/unsplit; a same-count session *swap* fits
    // the swapped-in session explicitly in the ActivateSession handler.
    if target == app.terminal_size {
        return;
    }

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
}

/// Add another connected session as a split pane (up to [`MAX_PANES`]).
/// Tile every open session (up to [`MAX_PANES`]) in the given orientation —
/// SecureCRT-style Tile Vertically / Horizontally / grid.
fn tile_all_sessions(app: &mut AditApp, mode: TileMode) {
    let ids: Vec<SessionId> = app
        .manager
        .sessions()
        .into_iter()
        .map(|summary| summary.id)
        .take(MAX_PANES)
        .collect();
    if ids.len() < 2 {
        app.notice = String::from("至少要两个会话才能平铺（先多连接/打开几个会话）");
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
fn untile_sessions(app: &mut AditApp) {
    app.panes.clear();
    app.focused_pane = 0;
    app.terminal_scroll_offset = 0;
    app.terminal_selection = None;
    sync_terminal_size(app);
    app.notice = String::from("已合并为单标签视图");
}

fn split_pane(app: &mut AditApp) {
    app.tile_mode = TileMode::Grid;
    // Seed the tiling from the active session on the first split.
    if app.panes.is_empty() {
        match app.manager.active_session() {
            Some(active) => {
                app.panes.push(active);
                app.focused_pane = 0;
            }
            None => {
                app.last_error = Some(String::from("请先连接一个会话再分屏"));
                return;
            }
        }
    }

    if app.panes.len() >= MAX_PANES {
        app.notice = format!("最多同时分屏 {MAX_PANES} 个终端");
        return;
    }

    // First open session not already shown in a pane.
    let candidate = app
        .manager
        .sessions()
        .into_iter()
        .map(|summary| summary.id)
        .find(|id| !app.panes.contains(id));

    let Some(session_id) = candidate else {
        app.panes.clear();
        app.focused_pane = 0;
        app.notice = String::from("没有更多会话可分屏（先在侧栏连接另一个会话）");
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
fn close_pane(app: &mut AditApp, index: usize) {
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
fn activate_session(app: &mut AditApp, session_id: SessionId) {
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
fn focus_pane(app: &mut AditApp, index: usize) {
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
fn sync_panes(app: &mut AditApp) {
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

/// Snapshot the persistable preferences from live app state.
fn current_settings(app: &AditApp) -> AppSettings {
    AppSettings {
        dark_mode: app.dark_mode,
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
    }
}

/// Persist settings when they drift from what is on disk. Called every Tick so
/// any config change (theme, folded groups, window size) is debounced into at
/// most one write per frame and survives a restart.
fn persist_settings_if_changed(app: &mut AditApp) {
    let current = current_settings(app);
    if current == app.persisted_settings {
        return;
    }
    if let Err(error) = app.settings_store.save(&current) {
        app.last_error = Some(format!("保存设置失败: {error}"));
    }
    // Update the baseline regardless of outcome so a failing write does not
    // retry on every frame.
    app.persisted_settings = current;
}

/// Keep the SFTP path-bar edit buffers in sync with each pane's current
/// directory, clearing the per-pane selection when the directory changes.
fn sync_sftp_state(app: &mut AditApp) {
    let Some((remote, local)) = app
        .manager
        .sftp_browser()
        .map(|browser| (browser.cwd.clone(), browser.local_cwd.display().to_string()))
    else {
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

fn view(app: &AditApp) -> Element<'_, Message> {
    DARK_MODE.store(app.dark_mode, Ordering::Relaxed);
    TERM_FONT.store(font_preset_index(&app.font_family), Ordering::Relaxed);
    TERM_FONT_SIZE.store(
        (app.font_size as u32).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE),
        Ordering::Relaxed,
    );
    TERM_SCHEME.store(color_scheme_index(&app.color_scheme), Ordering::Relaxed);

    let main = if app.sidebar_visible {
        row![sidebar(app), sidebar_divider(), workspace(app)]
    } else {
        row![workspace(app)]
    }
    .height(Fill)
    .width(Fill);

    let layout = column![menu_bar(app)]
        .push(toolbar(app))
        .push(main)
        .push(status_bar(app))
        .height(Fill)
        .width(Fill);

    let base: Element<'_, Message> = container(layout)
        .style(|_theme| app_background_style())
        .height(Fill)
        .width(Fill)
        .into();

    // Menus and the connection dialog float above the chrome instead of
    // reserving layout space, so opening one never shifts the content.
    let mut layers: Vec<Element<'_, Message>> = vec![base];
    if let Some(menu) = app.active_menu {
        layers.push(menu_overlay(menu));
    }
    if let Some(profile_id) = app.profile_context_menu {
        layers.push(opaque(profile_context_overlay(app, profile_id)));
    }
    if app.terminal_context_menu {
        layers.push(opaque(terminal_context_overlay(app)));
    }
    if app.profile_editor.is_some() {
        layers.push(opaque(profile_editor_overlay(app)));
    }
    if app.connection_dialog.is_some() {
        layers.push(opaque(connection_dialog_overlay(app)));
    }
    if let Some((session_id, prompt)) = app.manager.pending_host_key() {
        layers.push(opaque(host_key_dialog_overlay(session_id, &prompt)));
    }
    if app.manager.sftp_is_open() {
        layers.push(opaque(sftp_panel_overlay(app)));
    }
    if app.tunnels_open {
        layers.push(opaque(tunnels_panel_overlay(app)));
    }
    if app.about_open {
        layers.push(opaque(about_dialog_overlay()));
    }
    if app.update_dialog_open {
        layers.push(opaque(update_dialog_overlay(app)));
    }
    if app.paste_confirm_open {
        layers.push(opaque(paste_confirm_overlay(app)));
    }
    if app.renaming_session.is_some() {
        layers.push(opaque(session_rename_overlay(app)));
    }
    if app.snippets_open {
        layers.push(opaque(snippets_panel_overlay(app)));
    }
    if app.appearance_open {
        layers.push(opaque(appearance_dialog_overlay(app)));
    }
    if app.options_open {
        layers.push(opaque(options_dialog_overlay(app)));
    }

    if layers.len() == 1 {
        layers.pop().unwrap()
    } else {
        stack(layers).into()
    }
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
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .padding([3, 10])
    .height(MENU_BAR_HEIGHT)
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

fn menu_overlay(menu: MenuKind) -> Element<'static, Message> {
    let mut commands = column![].spacing(0).width(Fill);
    for (label, command) in menu_commands(menu) {
        commands = commands.push(menu_dropdown_button(label, *command));
    }

    // The dropdown card, positioned under its menu-bar button.
    let positioned = column![
        Space::new().height(Length::Fixed(MENU_BAR_HEIGHT)),
        row![
            Space::new().width(Length::Fixed(menu_dropdown_offset(menu))),
            container(commands)
                .width(Length::Fixed(182.0))
                .padding([5, 0])
                .style(|_theme| menu_dropdown_style()),
            Space::new().width(Fill),
        ],
        Space::new().height(Fill),
    ]
    .width(Fill)
    .height(Fill);

    // A click-catcher below the menu bar that dismisses the menu. It starts
    // below the bar so the other menu buttons stay clickable underneath.
    let backdrop = column![
        Space::new().height(Length::Fixed(MENU_BAR_HEIGHT)),
        mouse_area(container(Space::new()).width(Fill).height(Fill))
            .on_press(Message::ToggleMenu(menu)),
    ]
    .width(Fill)
    .height(Fill);

    stack(vec![backdrop.into(), positioned.into()]).into()
}

fn menu_commands(menu: MenuKind) -> &'static [(&'static str, MenuCommand)] {
    match menu {
        MenuKind::File => &[
            ("新建会话", MenuCommand::NewProfile),
            ("新建分组", MenuCommand::NewGroup),
            ("保存会话", MenuCommand::SaveProfile),
            ("删除会话", MenuCommand::DeleteProfile),
            ("导入 ~/.ssh/config", MenuCommand::ImportSshConfig),
            ("选项 / 日志…", MenuCommand::Options),
            ("关闭标签", MenuCommand::CloseActiveTab),
        ],
        MenuKind::Session => &[
            ("连接", MenuCommand::Connect),
            ("断开", MenuCommand::Disconnect),
            ("自动重连开关", MenuCommand::ToggleAutoReconnect),
            ("打开演示标签", MenuCommand::OpenMockTab),
            ("关闭标签", MenuCommand::CloseActiveTab),
        ],
        MenuKind::Edit => &[("清屏", MenuCommand::ClearTerminal)],
        MenuKind::View => &[
            ("外观设置…", MenuCommand::Appearance),
            ("垂直平铺（并排）", MenuCommand::TileVertical),
            ("水平平铺（上下）", MenuCommand::TileHorizontal),
            ("网格平铺", MenuCommand::TileGrid),
            ("合并为标签", MenuCommand::Untile),
            ("命令窗口", MenuCommand::ToggleCommandWindow),
            ("输入广播开关", MenuCommand::ToggleBroadcast),
            ("终端 96x28", MenuCommand::ResizeDefault),
            ("终端 120x36", MenuCommand::ResizeWide),
        ],
        MenuKind::Transfer => &[
            ("SFTP", MenuCommand::Sftp),
            ("端口转发", MenuCommand::Tunnels),
        ],
        MenuKind::Script => &[
            ("命令片段", MenuCommand::Snippets),
            ("日志/脚本", MenuCommand::Logging),
        ],
        MenuKind::Tools => &[
            ("清屏", MenuCommand::ClearTerminal),
            ("日志", MenuCommand::Logging),
        ],
        MenuKind::Help => &[
            ("检查更新…", MenuCommand::CheckUpdate),
            ("关于", MenuCommand::About),
        ],
    }
}

fn menu_dropdown_offset(menu: MenuKind) -> f32 {
    match menu {
        MenuKind::File => 28.0,
        MenuKind::Session => 72.0,
        MenuKind::Edit => 138.0,
        MenuKind::View => 184.0,
        MenuKind::Transfer => 236.0,
        MenuKind::Script => 318.0,
        MenuKind::Tools => 376.0,
        MenuKind::Help => 436.0,
    }
}

fn menu_dropdown_button(label: &'static str, command: MenuCommand) -> Element<'static, Message> {
    button(text(label).size(12))
        .width(Fill)
        .padding([6, 12])
        .style(|_theme, status| menu_command_button_style(status))
        .on_press(Message::RunMenu(command))
        .into()
}

fn toolbar(app: &AditApp) -> Element<'_, Message> {
    container(
        row![
            tool_button("☰", Message::ToggleSidebar),
            tool_separator(),
            tool_button("↯", Message::ConnectSelectedProfile),
            tool_button("■", Message::DisconnectActive),
            tool_button("+", Message::NewProfileDraft),
            tool_button("G+", Message::NewGroupDraft),
            tool_button("□", Message::SaveProfile),
            tool_button("×", Message::DeleteSelectedProfile),
            tool_separator(),
            tool_button("↺", Message::OpenSelectedProfile),
            tool_button("⌫", Message::ClearActiveTerminal),
            tool_button("⇅", Message::RunMenu(MenuCommand::Sftp)),
            tool_button("⇄", Message::OpenTunnels),
            tool_toggle_button("⇶", app.broadcast_input, Message::ToggleBroadcast),
            tool_toggle_button(">_", app.command_window_open, Message::ToggleCommandWindow),
            tool_separator(),
            text_input("Enter host <Alt+R>", &app.profile_host)
                .on_input(Message::ProfileHostChanged)
                .on_submit(Message::ConnectSelectedProfile)
                .padding([4, 8])
                .style(toolbar_input_style)
                .width(Length::Fixed(210.0)),
            button(text("Connect").size(13))
                .padding([5, 14])
                .style(|_theme, status| primary_button_style(status))
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
            theme_toggle_button(app),
        ]
        .spacing(5)
        .align_y(Alignment::Center),
    )
    .padding([4, 10])
    .height(TOOLBAR_HEIGHT)
    .width(Fill)
    .style(|_theme| toolbar_style())
    .into()
}

fn theme_toggle_button(app: &AditApp) -> Element<'static, Message> {
    let glyph = if app.dark_mode { "☀" } else { "☾" };
    button(text(glyph).size(14))
        .width(Length::Fixed(28.0))
        .height(Length::Fixed(26.0))
        .padding(0)
        .style(|_theme, status| toolbar_icon_button_style(status))
        .on_press(Message::ToggleTheme)
        .into()
}

fn tool_button(label: &'static str, message: Message) -> Element<'static, Message> {
    button(text(label).size(14))
        .width(Length::Fixed(28.0))
        .height(Length::Fixed(26.0))
        .padding(0)
        .style(|_theme, status| toolbar_icon_button_style(status))
        .on_press(message)
        .into()
}

/// A toolbar icon button that stays highlighted (accent fill) while `active`.
fn tool_toggle_button(
    label: &'static str,
    active: bool,
    message: Message,
) -> Element<'static, Message> {
    button(text(label).size(14))
        .width(Length::Fixed(28.0))
        .height(Length::Fixed(26.0))
        .padding(0)
        .style(move |_theme, status| {
            if active {
                base_button_style(accent(), Color::from_rgb8(245, 249, 255), transparent())
            } else {
                toolbar_icon_button_style(status)
            }
        })
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

fn connection_dialog_overlay(app: &AditApp) -> Element<'_, Message> {
    let Some(dialog) = app.connection_dialog.as_ref() else {
        return Space::new().width(Fill).height(Fill).into();
    };

    let auth_hint = match dialog.auth_method {
        AuthMethod::Auto => "自动认证：密码可选，会先尝试密码、agent 和默认密钥",
        AuthMethod::Password => "密码认证：请输入 SSH 密码",
        AuthMethod::Key => "密钥认证：如私钥有 passphrase，请在这里输入",
        AuthMethod::Agent => "Agent 认证：通常不需要密码",
    };

    let mut details = column![
        row![
            text("连接 SSH").size(16).color(primary_text()),
            Space::new().width(Fill),
            button("×")
                .width(Length::Fixed(26.0))
                .height(Length::Fixed(24.0))
                .padding(0)
                .style(|_theme, status| close_button_style(status))
                .on_press(Message::CancelConnection),
        ]
        .align_y(Alignment::Center),
        text(dialog.title.clone()).size(13).color(primary_text()),
        text(dialog.endpoint.clone()).size(12).color(muted_text()),
        text(auth_hint).size(11).color(muted_text()),
    ]
    .spacing(6);

    if !dialog.identity_file.trim().is_empty() {
        details = details.push(
            text(format!("Identity: {}", dialog.identity_file))
                .size(11)
                .color(muted_text()),
        );
    }

    let panel = container(
        column![
            details,
            text_input("Password / passphrase", &app.password)
                .secure(true)
                .on_input(Message::ConnectionPasswordChanged)
                .on_submit(Message::ConfirmConnection)
                .padding([6, 8])
                .style(text_input_style),
            checkbox(app.remember_connection_password)
                .label("保存到系统凭据库")
                .on_toggle(Message::RememberConnectionPasswordChanged)
                .size(14)
                .text_size(12)
                .spacing(8),
            text("仅保存到系统凭据库，不写入 profiles.json")
                .size(10)
                .color(muted_text()),
            row![
                button("取消")
                    .width(Fill)
                    .padding([6, 10])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::CancelConnection),
                button("连接")
                    .width(Fill)
                    .padding([6, 10])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::ConfirmConnection),
            ]
            .spacing(8),
        ]
        .spacing(12),
    )
    .width(Length::Fixed(430.0))
    .padding(14)
    .style(|_theme| connection_dialog_style());

    container(panel)
        .width(Fill)
        .height(Fill)
        .center(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}

fn host_key_dialog_overlay(
    session_id: SessionId,
    prompt: &HostKeyPromptInfo,
) -> Element<'static, Message> {
    let changed = prompt.previous_fingerprint.is_some();
    let title = if changed {
        "⚠ 主机密钥已变更"
    } else {
        "确认主机密钥"
    };
    let title_color = if changed { danger() } else { primary_text() };

    let mut details = column![
        text(title).size(16).color(title_color),
        text(format!("{}:{}", prompt.host, prompt.port))
            .size(13)
            .color(primary_text()),
        text(format!("密钥类型: {}", prompt.key_type))
            .size(11)
            .color(muted_text()),
        text("SHA256 指纹").size(11).color(muted_text()),
        text(prompt.fingerprint.clone())
            .size(12)
            .font(Font::MONOSPACE)
            .color(primary_text()),
    ]
    .spacing(6);

    if let Some(previous) = &prompt.previous_fingerprint {
        details = details
            .push(text("此前记录的指纹").size(11).color(muted_text()))
            .push(
                text(previous.clone())
                    .size(12)
                    .font(Font::MONOSPACE)
                    .color(danger()),
            )
            .push(
                text("密钥变更可能意味着中间人攻击。仅在你确知服务器更换过密钥时才接受。")
                    .size(11)
                    .color(danger()),
            );
    } else {
        details = details.push(
            text("首次连接此主机。请通过其它可信渠道核对指纹后再信任。")
                .size(11)
                .color(muted_text()),
        );
    }

    let accept_label = if changed {
        "更新并继续"
    } else {
        "信任并继续"
    };

    let panel = container(
        column![
            details,
            row![
                button("拒绝")
                    .width(Fill)
                    .padding([6, 10])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::RespondHostKey {
                        session_id,
                        accept: false
                    }),
                button(text(accept_label))
                    .width(Fill)
                    .padding([6, 10])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::RespondHostKey {
                        session_id,
                        accept: true
                    }),
            ]
            .spacing(8),
        ]
        .spacing(12),
    )
    .width(Length::Fixed(460.0))
    .padding(14)
    .style(|_theme| connection_dialog_style());

    container(panel)
        .width(Fill)
        .height(Fill)
        .center(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}

fn add_tunnel(app: &mut AditApp) {
    let bind_port: u16 = match app.tunnel_bind_port.trim().parse() {
        Ok(port) if port > 0 => port,
        _ => {
            app.last_error = Some(String::from("请输入有效的本地端口"));
            return;
        }
    };
    let (target_host, target_port) = match app.tunnel_kind {
        TunnelKind::Local | TunnelKind::Remote => {
            let host = app.tunnel_target_host.trim().to_string();
            if host.is_empty() {
                app.last_error = Some(String::from("该转发需要填写目标主机"));
                return;
            }
            match app.tunnel_target_port.trim().parse::<u16>() {
                Ok(port) if port > 0 => (host, port),
                _ => {
                    app.last_error = Some(String::from("请输入有效的目标端口"));
                    return;
                }
            }
        }
        TunnelKind::Dynamic => (String::new(), 0),
    };

    let bind_address = {
        let trimmed = app.tunnel_bind_addr.trim();
        if trimmed.is_empty() {
            String::from("127.0.0.1")
        } else {
            trimmed.to_string()
        }
    };

    match app.manager.open_tunnel(
        app.tunnel_kind,
        bind_address.clone(),
        bind_port,
        target_host.clone(),
        target_port,
    ) {
        Ok(()) => {
            app.last_error = None;
            app.notice = String::from("已创建端口转发");
            // Persist to the active profile so it auto-starts on the next connect.
            if app.tunnel_save {
                if let Some(profile_id) = app.manager.active_session_summary().map(|s| s.profile_id)
                {
                    app.manager.add_profile_tunnel(
                        profile_id,
                        TunnelDef {
                            kind: app.tunnel_kind,
                            bind_address,
                            bind_port,
                            target_host,
                            target_port,
                        },
                    );
                    persist_profiles(app);
                }
            }
            app.tunnel_bind_port.clear();
            app.tunnel_target_host.clear();
            app.tunnel_target_port.clear();
        }
        Err(error) => app.last_error = Some(format!("端口转发失败: {error}")),
    }
}

fn about_dialog_overlay() -> Element<'static, Message> {
    let version = env!("CARGO_PKG_VERSION");
    let card = container(
        column![
            row![
                text("Adit").size(20).color(primary_text()),
                Space::new().width(Fill),
                button("×")
                    .width(Length::Fixed(26.0))
                    .height(Length::Fixed(24.0))
                    .padding(0)
                    .style(|_theme, status| close_button_style(status))
                    .on_press(Message::CloseAbout),
            ]
            .align_y(Alignment::Center),
            text(format!("版本 v{version}")).size(13).color(accent()),
            text("原生 Rust 桌面 SSH 终端").size(13).color(primary_text()),
            text("iced · russh · vte 终端核心 — 无 WebView，无 JavaScript")
                .size(12)
                .color(muted_text()),
            text("github.com/weironz/adit").size(12).color(muted_text()),
            row![
                Space::new().width(Fill),
                button(text("确定").size(12))
                    .padding([5, 18])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::CloseAbout),
            ],
        ]
        .spacing(12),
    )
    .width(Length::Fixed(380.0))
    .padding(20)
    .style(|_theme| connection_dialog_style());

    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}

/// A single font-family choice button (label rendered in that very font).
fn appearance_font_button(index: usize, current: u8) -> Element<'static, Message> {
    let (label, family) = FONT_PRESETS[index];
    let selected = index as u8 == current;
    let font = match family {
        Some(name) => Font::with_name(name),
        None => Font::MONOSPACE,
    };
    button(text(label).size(12).font(font))
        .padding([5, 10])
        .width(Length::Fixed(134.0))
        .style(move |_theme, status| {
            if selected {
                primary_button_style(status)
            } else {
                secondary_button_style(status)
            }
        })
        .on_press(Message::FontFamilyChanged(index as u8))
        .into()
}

/// A color-scheme choice button: a background swatch plus the scheme name.
fn appearance_scheme_button(index: usize, current: u8) -> Element<'static, Message> {
    let scheme = &COLOR_SCHEMES[index];
    let selected = index as u8 == current;
    let (br, bg, bb) = scheme.background;
    let (fr, fg, fb) = scheme.ansi[2];
    let swatch = container(Space::new())
        .width(Length::Fixed(14.0))
        .height(Length::Fixed(14.0))
        .style(move |_theme| container::Style {
            background: Some(Background::Color(Color::from_rgb8(br, bg, bb))),
            border: Border {
                color: Color::from_rgb8(fr, fg, fb),
                width: 1.5,
                radius: 3.0.into(),
            },
            ..container::Style::default()
        });
    button(
        row![swatch, text(scheme.name).size(12)]
            .spacing(8)
            .align_y(Alignment::Center),
    )
    .padding([5, 10])
    .width(Length::Fixed(150.0))
    .style(move |_theme, status| {
        if selected {
            primary_button_style(status)
        } else {
            secondary_button_style(status)
        }
    })
    .on_press(Message::ColorSchemeChanged(index as u8))
    .into()
}

/// Chunk a flat list of built widgets into rows of `per_row`.
fn wrap_rows(mut buttons: Vec<Element<'static, Message>>, per_row: usize) -> Element<'static, Message> {
    let mut rows = column![].spacing(8);
    while !buttons.is_empty() {
        let take = buttons.len().min(per_row);
        let mut r = row![].spacing(8);
        for element in buttons.drain(0..take) {
            r = r.push(element);
        }
        rows = rows.push(r);
    }
    rows.into()
}

fn appearance_dialog_overlay(app: &AditApp) -> Element<'_, Message> {
    let current_font = font_preset_index(&app.font_family);
    let current_scheme = color_scheme_index(&app.color_scheme);
    let size = app.font_size as i32;

    let font_buttons: Vec<Element<'static, Message>> = (0..FONT_PRESETS.len())
        .map(|i| appearance_font_button(i, current_font))
        .collect();
    let scheme_buttons: Vec<Element<'static, Message>> = (0..COLOR_SCHEMES.len())
        .map(|i| appearance_scheme_button(i, current_scheme))
        .collect();

    let size_row = row![
        text("字号")
            .size(12)
            .color(muted_text())
            .width(Length::Fixed(52.0)),
        button(text("−").size(15))
            .width(Length::Fixed(32.0))
            .padding([2, 0])
            .style(|_theme, status| secondary_button_style(status))
            .on_press(Message::FontSizeStep(-1)),
        container(text(format!("{size} px")).size(13).color(primary_text()))
            .width(Length::Fixed(56.0))
            .center_x(Length::Fixed(56.0)),
        button(text("＋").size(15))
            .width(Length::Fixed(32.0))
            .padding([2, 0])
            .style(|_theme, status| secondary_button_style(status))
            .on_press(Message::FontSizeStep(1)),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    // Live preview — the static appearance is already set for this frame, so the
    // sample renders in exactly the chosen font + palette.
    let swatches = (0..16).fold(row![].spacing(2), |r, i| {
        r.push(
            container(Space::new())
                .width(Length::Fixed(13.0))
                .height(Length::Fixed(13.0))
                .style(move |_theme| container::Style {
                    background: Some(Background::Color(palette_color(i))),
                    border: Border {
                        radius: 2.0.into(),
                        ..Border::default()
                    },
                    ..container::Style::default()
                }),
        )
    });
    let preview = container(
        column![
            text("adit@host:~/project$  ls -la  AaBbCc 0123")
                .size(term_font_size())
                .font(term_font())
                .color(default_foreground()),
            swatches,
        ]
        .spacing(8),
    )
    .width(Fill)
    .padding(12)
    .style(|_theme| container::Style {
        background: Some(Background::Color(terminal_background())),
        border: Border {
            color: border_color(),
            width: 1.0,
            radius: RADIUS_SM.into(),
        },
        ..container::Style::default()
    });

    let card = container(
        column![
            row![
                text("外观设置").size(18).color(primary_text()),
                Space::new().width(Fill),
                button("×")
                    .width(Length::Fixed(26.0))
                    .height(Length::Fixed(24.0))
                    .padding(0)
                    .style(|_theme, status| close_button_style(status))
                    .on_press(Message::CloseAppearance),
            ]
            .align_y(Alignment::Center),
            text("字体").size(12).color(muted_text()),
            wrap_rows(font_buttons, 3),
            size_row,
            text("配色方案").size(12).color(muted_text()),
            wrap_rows(scheme_buttons, 3),
            text("预览").size(12).color(muted_text()),
            preview,
            row![
                Space::new().width(Fill),
                button(text("完成").size(12))
                    .padding([5, 18])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::CloseAppearance),
            ],
        ]
        .spacing(12),
    )
    .width(Length::Fixed(520.0))
    .padding(20)
    .style(|_theme| connection_dialog_style());

    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}

fn update_dialog_overlay(app: &AditApp) -> Element<'_, Message> {
    let current = env!("CARGO_PKG_VERSION");

    let body: Element<'_, Message> = match &app.update_state {
        UpdateState::Idle | UpdateState::Checking => {
            column![text("正在检查更新…").size(13).color(primary_text())].into()
        }
        UpdateState::UpToDate => column![
            text(format!("已是最新版本（v{current}）"))
                .size(13)
                .color(primary_text()),
        ]
        .into(),
        UpdateState::Available(info) => {
            let mut col = column![
                text(format!("发现新版本 {}", info.tag))
                    .size(15)
                    .color(accent()),
                text(format!("当前版本 v{current}"))
                    .size(12)
                    .color(muted_text()),
            ]
            .spacing(6);
            if !info.notes_url.is_empty() {
                col = col.push(
                    button(text("查看发布说明").size(12))
                        .padding([3, 0])
                        .style(|_theme, _status| {
                            base_button_style(transparent(), accent(), transparent())
                        })
                        .on_press(Message::OpenReleaseNotes(info.notes_url.clone())),
                );
            }
            let action = if info.installer_url.is_empty() {
                text("该版本暂无 Windows 安装包")
                    .size(12)
                    .color(muted_text())
                    .into()
            } else {
                let btn: Element<'_, Message> = button(text("下载并更新").size(12))
                    .padding([6, 18])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::StartUpdateDownload)
                    .into();
                btn
            };
            col.push(Space::new().height(Length::Fixed(4.0)))
                .push(action)
                .into()
        }
        UpdateState::Downloading => column![
            text("正在下载安装包…").size(13).color(primary_text()),
            text("完成后会自动启动安装程序")
                .size(11)
                .color(muted_text()),
        ]
        .spacing(6)
        .into(),
        UpdateState::Launched => column![
            text("正在后台安装更新…").size(13).color(primary_text()),
            text("无需操作，安装完成后 Adit 会自动关闭并重启（可能需要确认一次 UAC）")
                .size(11)
                .color(muted_text()),
        ]
        .spacing(6)
        .into(),
        UpdateState::Error(error) => column![
            text("检查/更新失败").size(13).color(danger()),
            text(error.clone()).size(11).color(muted_text()),
            button(text("重试").size(12))
                .padding([5, 16])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::CheckForUpdates),
        ]
        .spacing(8)
        .into(),
    };

    let card = container(
        column![
            row![
                text("检查更新").size(18).color(primary_text()),
                Space::new().width(Fill),
                button("×")
                    .width(Length::Fixed(26.0))
                    .height(Length::Fixed(24.0))
                    .padding(0)
                    .style(|_theme, status| close_button_style(status))
                    .on_press(Message::CloseUpdateDialog),
            ]
            .align_y(Alignment::Center),
            body,
            row![
                Space::new().width(Fill),
                button(text("关闭").size(12))
                    .padding([5, 18])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::CloseUpdateDialog),
            ],
        ]
        .spacing(16),
    )
    .width(Length::Fixed(420.0))
    .padding(20)
    .style(|_theme| connection_dialog_style());

    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}

/// Small dialog to rename the active session's tab.
fn session_rename_overlay(app: &AditApp) -> Element<'_, Message> {
    let card = container(
        column![
            text("重命名标签").size(16).color(primary_text()),
            text_input("标签名称", &app.session_rename_draft)
                .on_input(Message::SessionRenameChanged)
                .on_submit(Message::ConfirmRenameSession)
                .padding([5, 8])
                .style(text_input_style)
                .width(Fill),
            row![
                Space::new().width(Fill),
                button(text("取消").size(12))
                    .padding([5, 16])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::CancelRenameSession),
                button(text("确定").size(12))
                    .padding([5, 18])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::ConfirmRenameSession),
            ]
            .spacing(8),
        ]
        .spacing(12),
    )
    .width(Length::Fixed(380.0))
    .padding(20)
    .style(|_theme| connection_dialog_style());

    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}

/// Command-snippets panel: list saved commands (send / delete) + an add form.
fn snippets_panel_overlay(app: &AditApp) -> Element<'_, Message> {
    let header = row![
        text("命令片段").size(16).color(primary_text()),
        Space::new().width(Fill),
        button("×")
            .width(Length::Fixed(26.0))
            .height(Length::Fixed(24.0))
            .padding(0)
            .style(|_theme, status| close_button_style(status))
            .on_press(Message::CloseSnippets),
    ]
    .align_y(Alignment::Center);

    let mut list = column![].spacing(6);
    if app.snippets.is_empty() {
        list = list.push(
            text("还没有片段。在下方添加常用命令，一键发送到当前终端。")
                .size(11)
                .color(muted_text()),
        );
    }
    for (index, snippet) in app.snippets.iter().enumerate() {
        list = list.push(
            container(
                row![
                    column![
                        text(snippet.name.clone()).size(12).color(primary_text()),
                        text(snippet.command.clone()).size(11).color(muted_text()),
                    ]
                    .spacing(1)
                    .width(Fill),
                    button(text("发送").size(11))
                        .padding([4, 12])
                        .style(|_theme, status| primary_button_style(status))
                        .on_press(Message::SendSnippet(index)),
                    button(text("删除").size(11))
                        .padding([4, 10])
                        .style(|_theme, status| secondary_button_style(status))
                        .on_press(Message::DeleteSnippet(index)),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .padding([4, 6])
            .style(|_theme| sftp_pane_style()),
        );
    }

    let form = column![
        text("新增片段").size(12).color(muted_text()),
        text_input("名称（可选）", &app.snippet_name_draft)
            .on_input(Message::SnippetNameChanged)
            .padding([5, 8])
            .style(text_input_style)
            .width(Fill),
        row![
            text_input("命令，如 tail -f /var/log/syslog", &app.snippet_command_draft)
                .on_input(Message::SnippetCommandChanged)
                .on_submit(Message::AddSnippet)
                .padding([5, 8])
                .style(text_input_style)
                .width(Fill),
            button(text("添加").size(12))
                .padding([5, 16])
                .style(|_theme, status| primary_button_style(status))
                .on_press(Message::AddSnippet),
        ]
        .spacing(8),
    ]
    .spacing(6);

    let card = container(
        column![
            header,
            scrollable(list).height(Length::Fixed(240.0)),
            form,
        ]
        .spacing(14),
    )
    .width(Length::Fixed(560.0))
    .padding(20)
    .style(|_theme| connection_dialog_style());

    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}

/// Confirmation dialog shown before pasting multi-line clipboard text.
fn paste_confirm_overlay(app: &AditApp) -> Element<'_, Message> {
    let contents = app.pending_paste.as_deref().unwrap_or_default();
    let line_count = contents.lines().count().max(1);
    let preview: String = contents.lines().take(8).collect::<Vec<_>>().join("\n");
    let preview = if preview.chars().count() > 400 {
        format!("{}…", preview.chars().take(400).collect::<String>())
    } else {
        preview
    };

    let card = container(
        column![
            text("确认粘贴").size(16).color(primary_text()),
            text(format!("将向当前终端粘贴 {line_count} 行内容："))
                .size(12)
                .color(muted_text()),
            container(
                scrollable(text(preview).size(12).font(Font::MONOSPACE).color(primary_text()))
                    .height(Length::Fixed(140.0))
            )
            .width(Fill)
            .padding(10)
            .style(|_theme| container::Style {
                background: Some(Background::Color(terminal_background())),
                border: border(RADIUS_SM, 1.0, border_color()),
                ..container::Style::default()
            }),
            row![
                Space::new().width(Fill),
                button(text("取消").size(12))
                    .padding([5, 16])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::CancelPaste),
                button(text("粘贴").size(12))
                    .padding([5, 18])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::ConfirmPaste),
            ]
            .spacing(8),
        ]
        .spacing(12),
    )
    .width(Length::Fixed(480.0))
    .padding(20)
    .style(|_theme| connection_dialog_style());

    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}

/// A read-only path row: label + monospace path + an 打开 button.
fn options_path_row<'a>(
    label: &'a str,
    path: String,
    open: Option<Message>,
) -> Element<'a, Message> {
    let mut row = row![
        text(label)
            .size(11)
            .color(muted_text())
            .width(Length::Fixed(96.0)),
        container(text(path).size(12).font(Font::MONOSPACE).color(primary_text()))
            .width(Fill),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    if let Some(message) = open {
        row = row.push(
            button(text("打开").size(11))
                .padding([3, 12])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(message),
        );
    }
    row.into()
}

fn options_dialog_overlay(app: &AditApp) -> Element<'_, Message> {
    let config_dir = adit_storage::config_dir();
    let overridden = std::env::var_os("ADIT_CONFIG_DIR")
        .is_some_and(|value| !value.is_empty());

    let config_note = if overridden {
        "由环境变量 ADIT_CONFIG_DIR 指定"
    } else {
        "默认位置（设置环境变量 ADIT_CONFIG_DIR 可改到同步盘等其他目录，重启生效）"
    };

    let config_section = column![
        text("配置目录").size(13).color(primary_text()),
        options_path_row(
            "配置目录",
            config_dir.display().to_string(),
            Some(Message::OpenConfigFolder),
        ),
        options_path_row(
            "会话配置",
            config_dir.join("profiles.json").display().to_string(),
            None,
        ),
        options_path_row(
            "应用设置",
            config_dir.join("settings.json").display().to_string(),
            None,
        ),
        text(config_note).size(11).color(muted_text()),
        row![
            text("连接超时（秒，0 = 不限）")
                .size(12)
                .color(muted_text())
                .width(Length::Fixed(180.0)),
            text_input("20", &app.connect_timeout_secs.to_string())
                .on_input(Message::ConnectTimeoutChanged)
                .padding([4, 8])
                .style(text_input_style)
                .width(Length::Fixed(80.0)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        row![
            text("滚动历史行数")
                .size(12)
                .color(muted_text())
                .width(Length::Fixed(180.0)),
            text_input("5000", &app.scrollback_lines.to_string())
                .on_input(Message::ScrollbackLinesChanged)
                .padding([4, 8])
                .style(text_input_style)
                .width(Length::Fixed(80.0)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        checkbox(app.auto_check_updates)
            .label("启动时自动检查更新")
            .on_toggle(Message::ToggleAutoCheckUpdates)
            .size(16)
            .text_size(12),
    ]
    .spacing(8);

    // Live preview of the rendered log filename for the active (or a sample)
    // session.
    let sample = app
        .manager
        .active_session_summary()
        .map(|summary| (summary.title, summary.endpoint))
        .unwrap_or_else(|| (String::from("web01"), String::from("root@10.0.0.5:22")));
    let preview_name = render_log_name(&effective_log_pattern(app), &sample.0, &sample.1);
    let preview_path = effective_log_dir(app).join(&preview_name);

    let log_section = column![
        text("会话日志").size(13).color(primary_text()),
        column![
            text("日志目录（留空 = 配置目录下的 logs）")
                .size(11)
                .color(muted_text()),
            row![
                text_input(
                    &adit_storage::default_log_dir().display().to_string(),
                    &app.log_dir,
                )
                .on_input(Message::LogDirChanged)
                .padding([5, 8])
                .style(text_input_style)
                .width(Fill),
                button(text("打开").size(11))
                    .padding([5, 12])
                    .style(|_theme, status| secondary_button_style(status))
                    .on_press(Message::OpenLogFolder),
            ]
            .spacing(8),
        ]
        .spacing(3),
        column![
            text("日志文件名（留空 = 默认）").size(11).color(muted_text()),
            text_input(DEFAULT_LOG_PATTERN, &app.log_name_pattern)
                .on_input(Message::LogNamePatternChanged)
                .padding([5, 8])
                .style(text_input_style)
                .width(Fill),
        ]
        .spacing(3),
        text("可用变量：%N 会话名  %H 主机  %Y 年 %M 月 %D 日  %h 时 %m 分 %s 秒")
            .size(11)
            .color(muted_text()),
        options_path_row("预览", preview_path.display().to_string(), None),
        checkbox(app.auto_log_on_connect)
            .label("连接后自动开始记录日志")
            .on_toggle(Message::ToggleAutoLog)
            .size(16)
            .text_size(12),
        checkbox(app.log_plaintext)
            .label("记录为纯文本（去除颜色/转义码，便于阅读和 grep）")
            .on_toggle(Message::ToggleLogPlaintext)
            .size(16)
            .text_size(12),
    ]
    .spacing(8);

    let mouse_section = column![
        text("终端复制 / 粘贴（PuTTY 风格）")
            .size(13)
            .color(primary_text()),
        checkbox(app.copy_on_select)
            .label("选中内容即复制到剪贴板")
            .on_toggle(Message::ToggleCopyOnSelect)
            .size(16)
            .text_size(12),
        checkbox(app.right_click_paste)
            .label("右键直接粘贴（不弹出菜单）")
            .on_toggle(Message::ToggleRightClickPaste)
            .size(16)
            .text_size(12),
        checkbox(app.confirm_multiline_paste)
            .label("粘贴多行内容前先确认")
            .on_toggle(Message::ToggleConfirmMultilinePaste)
            .size(16)
            .text_size(12),
        text("提示：右键粘贴开启后，清屏 / 回到底部可用工具栏或 Edit 菜单。程序也支持 bracketed paste（应用开启后粘贴不会被自动执行）。")
            .size(11)
            .color(muted_text()),
    ]
    .spacing(8);

    let divider = || {
        container(Space::new().height(Length::Fixed(1.0)).width(Fill)).style(|_theme| {
            container::Style {
                background: Some(Background::Color(border_color())),
                ..container::Style::default()
            }
        })
    };

    let card = container(
        column![
            row![
                text("选项").size(18).color(primary_text()),
                Space::new().width(Fill),
                button("×")
                    .width(Length::Fixed(26.0))
                    .height(Length::Fixed(24.0))
                    .padding(0)
                    .style(|_theme, status| close_button_style(status))
                    .on_press(Message::CloseOptions),
            ]
            .align_y(Alignment::Center),
            config_section,
            divider(),
            log_section,
            divider(),
            mouse_section,
            row![
                Space::new().width(Fill),
                button(text("完成").size(12))
                    .padding([5, 18])
                    .style(|_theme, status| primary_button_style(status))
                    .on_press(Message::CloseOptions),
            ],
        ]
        .spacing(14),
    )
    .width(Length::Fixed(560.0))
    .padding(20)
    .style(|_theme| connection_dialog_style());

    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .style(|_theme| dialog_scrim_style())
        .into()
}

fn tunnels_panel_overlay(app: &AditApp) -> Element<'_, Message> {
    let endpoint = app
        .manager
        .active_session_summary()
        .map(|summary| summary.endpoint)
        .unwrap_or_default();

    let header = row![
        text("端口转发").size(15).color(primary_text()),
        text(endpoint).size(11).color(muted_text()),
        Space::new().width(Fill),
        button("×")
            .width(Length::Fixed(26.0))
            .height(Length::Fixed(24.0))
            .padding(0)
            .style(|_theme, status| close_button_style(status))
            .on_press(Message::CloseTunnels),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let kind_row = row![
        text("类型").size(12).color(muted_text()).width(Length::Fixed(52.0)),
        tunnel_kind_button("本地转发 -L", TunnelKind::Local, app.tunnel_kind),
        tunnel_kind_button("动态 SOCKS -D", TunnelKind::Dynamic, app.tunnel_kind),
        tunnel_kind_button("远程转发 -R", TunnelKind::Remote, app.tunnel_kind),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let hint = match app.tunnel_kind {
        TunnelKind::Local => "本机端口 → 经 SSH 服务器 → 目标地址（访问服务器能到达的内网服务）",
        TunnelKind::Dynamic => "本机启动 SOCKS5 代理，应用挂上后所有流量走服务器出口",
        TunnelKind::Remote => "服务器监听端口 → 经 SSH 隧道 → 本机目标地址（把本地服务暴露给远端网络）",
    };

    let bind_label = if app.tunnel_kind == TunnelKind::Remote {
        "远端"
    } else {
        "本地"
    };
    let bind_placeholder = if app.tunnel_kind == TunnelKind::Remote {
        "127.0.0.1（远端绑定，0.0.0.0 对外）"
    } else {
        "127.0.0.1"
    };

    let bind_row = row![
        text(bind_label).size(12).color(muted_text()).width(Length::Fixed(52.0)),
        text_input(bind_placeholder, &app.tunnel_bind_addr)
            .on_input(Message::TunnelBindAddrChanged)
            .padding([4, 8])
            .style(text_input_style)
            .width(Length::Fixed(150.0)),
        text(":").size(12).color(muted_text()),
        text_input("端口", &app.tunnel_bind_port)
            .on_input(Message::TunnelBindPortChanged)
            .on_submit(Message::AddTunnel)
            .padding([4, 8])
            .style(text_input_style)
            .width(Length::Fixed(90.0)),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut form = column![kind_row, text(hint).size(10).color(muted_text()), bind_row].spacing(8);

    if app.tunnel_kind != TunnelKind::Dynamic {
        let target_label = if app.tunnel_kind == TunnelKind::Remote {
            "本地"
        } else {
            "目标"
        };
        form = form.push(
            row![
                text(target_label).size(12).color(muted_text()).width(Length::Fixed(52.0)),
                text_input("目标主机（如 10.0.0.5）", &app.tunnel_target_host)
                    .on_input(Message::TunnelTargetHostChanged)
                    .padding([4, 8])
                    .style(text_input_style)
                    .width(Fill),
                text(":").size(12).color(muted_text()),
                text_input("端口", &app.tunnel_target_port)
                    .on_input(Message::TunnelTargetPortChanged)
                    .on_submit(Message::AddTunnel)
                    .padding([4, 8])
                    .style(text_input_style)
                    .width(Length::Fixed(90.0)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }

    form = form.push(
        row![
            checkbox(app.tunnel_save)
                .label("保存到会话配置（连接时自动开启）")
                .on_toggle(Message::ToggleTunnelSave)
                .size(15)
                .text_size(11),
            Space::new().width(Fill),
            button(text("添加转发").size(12))
                .padding([5, 16])
                .style(|_theme, status| primary_button_style(status))
                .on_press(Message::AddTunnel),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    );

    let tunnels = app.manager.tunnels();
    let mut list = column![].spacing(2);
    if tunnels.is_empty() {
        list = list.push(text("（暂无转发）").size(11).color(muted_text()));
    } else {
        for tunnel in tunnels {
            list = list.push(tunnel_row(tunnel));
        }
    }

    // Saved (auto-start) definitions for the active profile.
    let saved: Vec<TunnelDef> = app
        .manager
        .active_session_summary()
        .and_then(|summary| {
            app.manager
                .profile(summary.profile_id)
                .map(|profile| profile.tunnels.clone())
        })
        .unwrap_or_default();
    let mut saved_list = column![].spacing(2);
    if saved.is_empty() {
        saved_list = saved_list.push(text("（无）").size(11).color(muted_text()));
    } else {
        for (index, def) in saved.iter().enumerate() {
            saved_list = saved_list.push(saved_tunnel_row(index, def));
        }
    }

    let content = column![
        header,
        container(form)
            .padding(12)
            .width(Fill)
            .style(|_theme| sftp_pane_style()),
        text("已保存（连接时自动开启）").size(12).color(primary_text()),
        container(saved_list)
            .padding(8)
            .width(Fill)
            .style(|_theme| sftp_list_inner_style()),
        text("活动转发").size(12).color(primary_text()),
        container(scrollable(list).height(Fill))
            .height(Fill)
            .padding(6)
            .style(|_theme| sftp_list_inner_style()),
    ]
    .spacing(10);

    let panel = container(content)
        .width(Fill)
        .height(Fill)
        .padding(16)
        .style(|_theme| connection_dialog_style());

    container(panel)
        .width(Fill)
        .height(Fill)
        .padding(48)
        .style(|_theme| dialog_scrim_style())
        .into()
}

fn tunnel_kind_button(
    label: &'static str,
    kind: TunnelKind,
    current: TunnelKind,
) -> Element<'static, Message> {
    let selected = kind == current;
    button(text(label).size(12))
        .padding([5, 14])
        .style(move |_theme, status| {
            if selected {
                primary_button_style(status)
            } else {
                secondary_button_style(status)
            }
        })
        .on_press(Message::TunnelKindChanged(kind))
        .into()
}

fn tunnel_row(tunnel: &TunnelState) -> Element<'static, Message> {
    let kind = match tunnel.kind {
        TunnelKind::Local => "L",
        TunnelKind::Dynamic => "D",
        TunnelKind::Remote => "R",
    };
    let route = match tunnel.kind {
        TunnelKind::Local => format!("{} → {}", tunnel.bind, tunnel.target),
        TunnelKind::Dynamic => format!("{}  (SOCKS5)", tunnel.bind),
        TunnelKind::Remote => format!("远端 {} → 本地 {}", tunnel.bind, tunnel.target),
    };
    let status_color = if tunnel.error.is_some() {
        danger()
    } else if tunnel.listening {
        Color::from_rgb8(34, 197, 94)
    } else {
        muted_text()
    };

    container(
        row![
            text(kind).size(11).color(accent()).width(Length::Fixed(18.0)),
            text(route).size(12).color(primary_text()).width(Fill),
            text(format!("活动 {}", tunnel.active))
                .size(10)
                .color(muted_text())
                .width(Length::Fixed(60.0)),
            text(tunnel.status.clone())
                .size(10)
                .color(status_color)
                .width(Length::Fixed(190.0)),
            button(text("关闭").size(11))
                .padding([3, 10])
                .style(|_theme, status| close_button_style(status))
                .on_press(Message::CloseTunnel(tunnel.id)),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding([4, 8])
    .into()
}

fn saved_tunnel_row(index: usize, def: &TunnelDef) -> Element<'static, Message> {
    let label = match def.kind {
        TunnelKind::Local => format!(
            "L  {}:{} → {}:{}",
            def.bind_address, def.bind_port, def.target_host, def.target_port
        ),
        TunnelKind::Dynamic => format!("D  {}:{}  (SOCKS5)", def.bind_address, def.bind_port),
        TunnelKind::Remote => format!(
            "R  远端 {}:{} → 本地 {}:{}",
            def.bind_address, def.bind_port, def.target_host, def.target_port
        ),
    };
    row![
        text(label).size(11).color(primary_text()).width(Fill),
        button(text("删除").size(11))
            .padding([3, 10])
            .style(|_theme, status| close_button_style(status))
            .on_press(Message::RemoveSavedTunnel(index)),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .padding([2, 8])
    .into()
}

fn sftp_panel_overlay(app: &AditApp) -> Element<'_, Message> {
    let Some(browser) = app.manager.sftp_browser() else {
        return Space::new().width(Fill).height(Fill).into();
    };

    // While dragging, the status line becomes a prominent drag hint.
    let (status_text, status_color) = match &app.sftp_drag {
        Some((src, name)) => {
            let count = match src {
                SftpPane::Local => app.sftp_local_selected.len(),
                SftpPane::Remote => app.sftp_remote_selected.len(),
            };
            let selected = match src {
                SftpPane::Local => app.sftp_local_selected.contains(name),
                SftpPane::Remote => app.sftp_remote_selected.contains(name),
            };
            let subject = if selected && count > 1 {
                format!("{count} 项")
            } else {
                format!("«{name}»")
            };
            let target = match src {
                SftpPane::Local => "松开到右侧远程栏上传",
                SftpPane::Remote => "松开到左侧本地栏下载",
            };
            (format!("⠿ 拖拽 {subject} — {target}"), accent())
        }
        None if browser.status.starts_with("error") => (browser.status.clone(), danger()),
        None => (browser.status.clone(), muted_text()),
    };

    let header = row![
        text(format!("SFTP — {}", browser.endpoint))
            .size(15)
            .color(primary_text()),
        Space::new().width(Fill),
        text(status_text).size(11).color(status_color),
        Space::new().width(Length::Fixed(12.0)),
        button("×")
            .width(Length::Fixed(26.0))
            .height(Length::Fixed(24.0))
            .padding(0)
            .style(|_theme, status| close_button_style(status))
            .on_press(Message::CloseSftp),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let panes = row![sftp_local_pane(app, browser), sftp_remote_pane(app, browser)]
        .spacing(10)
        .height(Fill);

    let mut panel_body = column![header].spacing(10);
    if let Some((_, from)) = &app.sftp_rename {
        panel_body = panel_body.push(sftp_rename_bar(from, &app.sftp_rename_to));
    }
    if let Some((_, name, _)) = &app.sftp_delete_target {
        panel_body = panel_body.push(sftp_delete_bar(name));
    }

    // Extra upload via picker / typed path → remote current directory.
    let upload_extra = row![
        button(text("选择文件上传…").size(12))
            .padding([5, 12])
            .style(|_theme, status| primary_button_style(status))
            .on_press(Message::SftpPickUpload),
        text_input("或输入本地路径上传到远程当前目录", &app.sftp_upload_path)
            .on_input(Message::SftpUploadPathChanged)
            .on_submit(Message::SftpUpload)
            .padding([5, 8])
            .style(text_input_style)
            .width(Fill),
        button(text("上传").size(12))
            .padding([5, 12])
            .style(|_theme, status| secondary_button_style(status))
            .on_press(Message::SftpUpload),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let panel_body = panel_body
        .push(panes)
        .push(upload_extra)
        .push(sftp_transfer_queue(browser));

    let panel = container(panel_body)
        .width(Fill)
        .height(Fill)
        .padding(14)
        .style(|_theme| connection_dialog_style());

    container(panel)
        .width(Fill)
        .height(Fill)
        .padding(20)
        .style(|_theme| dialog_scrim_style())
        .into()
}

fn sftp_local_pane<'a>(app: &'a AditApp, browser: &'a SftpBrowser) -> Element<'a, Message> {
    let header = row![
        text("本地").size(13).color(primary_text()),
        button(text("↑").size(12))
            .padding([3, 9])
            .style(|_theme, status| secondary_button_style(status))
            .on_press(Message::SftpLocalUp),
        button(text("⟳").size(12))
            .padding([3, 9])
            .style(|_theme, status| secondary_button_style(status))
            .on_press(Message::SftpLocalRefresh),
        text_input("本地路径（回车跳转）", &app.sftp_local_path_edit)
            .on_input(Message::SftpLocalPathChanged)
            .on_submit(Message::SftpLocalGo)
            .padding([3, 6])
            .style(toolbar_input_style)
            .width(Fill),
        sftp_batch_button(
            "上传选中",
            app.sftp_local_selected.len(),
            Message::SftpTransferSelected(SftpPane::Local),
        ),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let (key, ascending) = app.sftp_local_sort;
    let mut items: Vec<&LocalEntry> = browser.local_entries.iter().collect();
    items.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            sftp_cmp(
                key,
                ascending,
                (&a.name, a.size, a.mtime),
                (&b.name, b.size, b.mtime),
            )
        })
    });

    let mut list = column![sftp_nav_row("../", Message::SftpLocalUp)].spacing(1);
    for entry in items {
        let selected = app.sftp_local_selected.contains(&entry.name);
        list = list.push(sftp_local_entry_row(entry, selected));
    }

    let drop_active = app.sftp_drag.as_ref().map(|(p, _)| *p) == Some(SftpPane::Remote)
        && app.sftp_drag_over == Some(SftpPane::Local);

    let pane = container(
        column![
            header,
            sftp_sort_header(SftpPane::Local, app.sftp_local_sort),
            container(scrollable(list).height(Fill))
                .height(Fill)
                .padding(3)
                .style(|_theme| sftp_list_inner_style()),
        ]
        .spacing(6),
    )
    .width(Length::FillPortion(1))
    .height(Fill)
    .padding(8)
    .style(move |_theme| sftp_pane_style_dropzone(drop_active));

    sftp_drag_layer(app, SftpPane::Local, pane.into())
}

fn sftp_remote_pane<'a>(app: &'a AditApp, browser: &'a SftpBrowser) -> Element<'a, Message> {
    let header = row![
        text("远程").size(13).color(primary_text()),
        button(text("↑").size(12))
            .padding([3, 9])
            .style(|_theme, status| secondary_button_style(status))
            .on_press(Message::SftpUp),
        button(text("⟳").size(12))
            .padding([3, 9])
            .style(|_theme, status| secondary_button_style(status))
            .on_press(Message::SftpRefresh),
        text_input("远程路径（回车跳转）", &app.sftp_remote_path_edit)
            .on_input(Message::SftpRemotePathChanged)
            .on_submit(Message::SftpRemoteGo)
            .padding([3, 6])
            .style(toolbar_input_style)
            .width(Fill),
        sftp_batch_button(
            "下载选中",
            app.sftp_remote_selected.len(),
            Message::SftpTransferSelected(SftpPane::Remote),
        ),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let (key, ascending) = app.sftp_remote_sort;
    let mut items: Vec<&SftpEntry> = browser.entries.iter().collect();
    items.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            sftp_cmp(
                key,
                ascending,
                (&a.name, a.size, a.mtime.map(u64::from)),
                (&b.name, b.size, b.mtime.map(u64::from)),
            )
        })
    });

    let mut content = column![header, sftp_sort_header(SftpPane::Remote, app.sftp_remote_sort)]
        .spacing(6);

    let mut list = column![sftp_nav_row("../", Message::SftpUp)].spacing(1);
    for entry in items {
        let selected = app.sftp_remote_selected.contains(&entry.name);
        list = list.push(sftp_remote_entry_row(entry, selected));
    }
    content = content.push(
        container(scrollable(list).height(Fill))
            .height(Fill)
            .padding(3)
            .style(|_theme| sftp_list_inner_style()),
    );

    content = content.push(
        row![
            text_input("新文件夹名", &app.sftp_new_folder)
                .on_input(Message::SftpNewFolderChanged)
                .on_submit(Message::SftpMkdir)
                .padding([4, 8])
                .style(text_input_style)
                .width(Fill),
            button(text("新建").size(11))
                .padding([4, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::SftpMkdir),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    );

    let drop_active = app.sftp_drag.as_ref().map(|(p, _)| *p) == Some(SftpPane::Local)
        && app.sftp_drag_over == Some(SftpPane::Remote);

    let pane = container(content)
        .width(Length::FillPortion(1))
        .height(Fill)
        .padding(8)
        .style(move |_theme| sftp_pane_style_dropzone(drop_active));

    sftp_drag_layer(app, SftpPane::Remote, pane.into())
}

/// Rename bar shown at the panel level for whichever pane is being edited.
fn sftp_rename_bar<'a>(from: &str, rename_to: &'a str) -> Element<'a, Message> {
    container(
        row![
            text(format!("重命名 {from} →"))
                .size(12)
                .color(primary_text()),
            text_input("新名称", rename_to)
                .on_input(Message::SftpRenameToChanged)
                .on_submit(Message::SftpConfirmRename)
                .padding([4, 8])
                .style(text_input_style)
                .width(Fill),
            button(text("确定").size(11))
                .padding([4, 10])
                .style(|_theme, status| primary_button_style(status))
                .on_press(Message::SftpConfirmRename),
            button(text("取消").size(11))
                .padding([4, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::SftpCancelRename),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding(6)
    .style(|_theme| profile_edit_menu_style())
    .into()
}

/// Delete-confirmation bar shown at the panel level for whichever pane is being edited.
fn sftp_delete_bar(name: &str) -> Element<'static, Message> {
    container(
        row![
            text(format!("确认删除 {name}?"))
                .size(12)
                .color(danger())
                .width(Fill),
            button(text("删除").size(11))
                .padding([4, 10])
                .style(|_theme, status| primary_button_style(status))
                .on_press(Message::SftpConfirmDelete),
            button(text("取消").size(11))
                .padding([4, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::SftpCancelDelete),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding(6)
    .style(|_theme| error_panel_style())
    .into()
}

fn sftp_transfer_queue(browser: &SftpBrowser) -> Element<'static, Message> {
    let mut done = 0usize;
    let mut failed = 0usize;
    let mut active = 0usize;
    for item in &browser.transfers {
        match item.status {
            TransferStatus::Done => done += 1,
            TransferStatus::Failed => failed += 1,
            TransferStatus::Pending | TransferStatus::Active => active += 1,
        }
    }

    let mut clear = button(text("清空已完成").size(11))
        .padding([3, 10])
        .style(|_theme, status| secondary_button_style(status));
    if done + failed > 0 {
        clear = clear.on_press(Message::SftpClearTransfers);
    }

    let title = row![
        text("传输队列").size(11).color(primary_text()),
        text(format!("完成 {done} · 失败 {failed} · 进行 {active}"))
            .size(10)
            .color(muted_text()),
        Space::new().width(Fill),
        clear,
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let column_header = row![
        text("文件").size(10).color(muted_text()).width(Length::FillPortion(2)),
        text("目标路径").size(10).color(muted_text()).width(Length::FillPortion(3)),
        text("大小").size(10).color(muted_text()).width(Length::Fixed(72.0)),
        text("进度").size(10).color(muted_text()).width(Length::Fixed(112.0)),
        text("速度").size(10).color(muted_text()).width(Length::Fixed(78.0)),
        text("状态").size(10).color(muted_text()).width(Length::Fixed(48.0)),
    ]
    .spacing(8);

    let body: Element<'static, Message> = if browser.transfers.is_empty() {
        text("（暂无传输）").size(11).color(muted_text()).into()
    } else {
        let mut list = column![].spacing(1);
        for item in browser.transfers.iter().rev() {
            list = list.push(sftp_transfer_row(item));
        }
        scrollable(list).height(Length::Fixed(108.0)).into()
    };

    container(column![title, column_header, body].spacing(4))
        .width(Fill)
        .padding(8)
        .style(|_theme| sftp_pane_style())
        .into()
}

fn sftp_transfer_row(item: &TransferItem) -> Element<'static, Message> {
    let arrow = match item.direction {
        TransferDirection::Upload => "↑",
        TransferDirection::Download => "↓",
    };
    let (label, color) = match item.status {
        TransferStatus::Pending => ("排队", muted_text()),
        TransferStatus::Active => ("传输中", accent()),
        TransferStatus::Done => ("完成", Color::from_rgb8(34, 197, 94)),
        TransferStatus::Failed => ("失败", danger()),
    };
    // A completed transfer is always 100% — including 0-byte files, where
    // dividing by total would otherwise leave it at 0%.
    let (fraction, pct) = if matches!(item.status, TransferStatus::Done) {
        (1.0, 100)
    } else if item.total > 0 {
        (
            (item.done as f32 / item.total as f32).clamp(0.0, 1.0),
            item.done.saturating_mul(100).checked_div(item.total).unwrap_or(0),
        )
    } else {
        (0.0, 0)
    };
    let speed = if item.bps > 0 {
        format!("{}/s", human_size(item.bps))
    } else {
        String::from("—")
    };
    // On failure, show the reason in place of the destination so it's visible.
    let (detail, detail_color) = match (&item.status, &item.error) {
        (TransferStatus::Failed, Some(reason)) => (reason.clone(), danger()),
        _ => (item.dest.clone(), muted_text()),
    };

    let progress = row![
        progress_bar(0.0..=1.0, fraction)
            .length(Length::Fixed(70.0))
            .girth(Length::Fixed(6.0)),
        text(format!("{pct}%"))
            .size(9)
            .color(muted_text())
            .width(Length::Fixed(34.0)),
    ]
    .spacing(4)
    .align_y(Alignment::Center);

    row![
        row![
            text(arrow).size(10).color(muted_text()),
            text(item.name.clone()).size(10).color(primary_text()),
        ]
        .spacing(4)
        .width(Length::FillPortion(2)),
        text(detail)
            .size(10)
            .color(detail_color)
            .width(Length::FillPortion(3)),
        text(human_size(item.total))
            .size(10)
            .color(muted_text())
            .width(Length::Fixed(72.0)),
        container(progress).width(Length::Fixed(112.0)),
        text(speed).size(10).color(muted_text()).width(Length::Fixed(78.0)),
        text(label).size(10).color(color).width(Length::Fixed(48.0)),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn sftp_nav_row(label: &'static str, message: Message) -> Element<'static, Message> {
    button(text(label).size(12).color(accent()))
        .width(Fill)
        .padding([4, 8])
        .style(|_theme, status| sftp_entry_button_style(status))
        .on_press(message)
        .into()
}

fn sftp_local_entry_row(entry: &LocalEntry, selected: bool) -> Element<'static, Message> {
    let owned = entry.name.clone();
    if entry.is_dir {
        return row![
            button(text(format!("{}/", entry.name)).size(12).color(accent()))
                .width(Fill)
                .padding([4, 8])
                .style(|_theme, status| sftp_entry_button_style(status))
                .on_press(Message::SftpLocalNavigate(owned.clone())),
            text("DIR").size(10).color(muted_text()).width(Length::Fixed(64.0)),
            text(sftp_date(entry.mtime))
                .size(10)
                .color(muted_text())
                .width(Length::Fixed(118.0)),
            sftp_action(
                "重命名",
                Message::SftpBeginRename(SftpPane::Local, owned.clone()),
                false,
            ),
            sftp_action(
                "删除",
                Message::SftpBeginDelete(SftpPane::Local, owned, true),
                true,
            ),
        ]
        .spacing(4)
        .align_y(Alignment::Center)
        .into();
    }

    // File: click to select, double-click to upload.
    row![
        mouse_area(
            container(
                row![
                    text(entry.name.clone())
                        .size(12)
                        .color(primary_text())
                        .width(Fill),
                    text(human_size(entry.size))
                        .size(10)
                        .color(muted_text())
                        .width(Length::Fixed(64.0)),
                    text(sftp_date(entry.mtime))
                        .size(10)
                        .color(muted_text())
                        .width(Length::Fixed(118.0)),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .width(Fill)
            .padding([4, 8])
            .style(move |_theme| sftp_row_highlight(selected)),
        )
        .on_press(Message::SftpRowPress(SftpPane::Local, owned.clone())),
        sftp_action("上传 ↑", Message::SftpUploadLocal(owned.clone()), false),
        sftp_action(
            "重命名",
            Message::SftpBeginRename(SftpPane::Local, owned.clone()),
            false,
        ),
        sftp_action(
            "删除",
            Message::SftpBeginDelete(SftpPane::Local, owned, false),
            true,
        ),
    ]
    .spacing(4)
    .align_y(Alignment::Center)
    .into()
}

fn sftp_remote_entry_row(entry: &SftpEntry, selected: bool) -> Element<'static, Message> {
    let owned = entry.name.clone();
    if entry.is_dir {
        return row![
            button(text(format!("{}/", entry.name)).size(12).color(accent()))
                .width(Fill)
                .padding([4, 8])
                .style(|_theme, status| sftp_entry_button_style(status))
                .on_press(Message::SftpNavigate(owned.clone())),
            text("DIR").size(10).color(muted_text()).width(Length::Fixed(64.0)),
            text(sftp_date(entry.mtime.map(u64::from)))
                .size(10)
                .color(muted_text())
                .width(Length::Fixed(118.0)),
            sftp_action(
                "重命名",
                Message::SftpBeginRename(SftpPane::Remote, owned.clone()),
                false,
            ),
            sftp_action(
                "删除",
                Message::SftpBeginDelete(SftpPane::Remote, owned, true),
                true,
            ),
        ]
        .spacing(4)
        .align_y(Alignment::Center)
        .into();
    }

    // File: click to select, double-click to download.
    row![
        mouse_area(
            container(
                row![
                    text(entry.name.clone())
                        .size(12)
                        .color(primary_text())
                        .width(Fill),
                    text(human_size(entry.size))
                        .size(10)
                        .color(muted_text())
                        .width(Length::Fixed(64.0)),
                    text(sftp_date(entry.mtime.map(u64::from)))
                        .size(10)
                        .color(muted_text())
                        .width(Length::Fixed(118.0)),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .width(Fill)
            .padding([4, 8])
            .style(move |_theme| sftp_row_highlight(selected)),
        )
        .on_press(Message::SftpRowPress(SftpPane::Remote, owned.clone())),
        sftp_action("下载 ↓", Message::SftpDownload(owned.clone()), false),
        sftp_action(
            "重命名",
            Message::SftpBeginRename(SftpPane::Remote, owned.clone()),
            false,
        ),
        sftp_action(
            "删除",
            Message::SftpBeginDelete(SftpPane::Remote, owned, false),
            true,
        ),
    ]
    .spacing(4)
    .align_y(Alignment::Center)
    .into()
}

fn sftp_row_highlight(selected: bool) -> container::Style {
    container::Style {
        background: Some(Background::Color(if selected {
            accent_soft()
        } else {
            transparent()
        })),
        ..container::Style::default()
    }
}

fn sftp_action(label: &'static str, message: Message, danger: bool) -> Element<'static, Message> {
    button(text(label).size(11))
        .padding([3, 8])
        .style(move |_theme, status| {
            if danger {
                close_button_style(status)
            } else {
                secondary_button_style(status)
            }
        })
        .on_press(message)
        .into()
}

/// A batch-action button that shows the selection count and is disabled (no
/// `on_press`) when nothing is selected.
fn sftp_batch_button(label: &'static str, count: usize, message: Message) -> Element<'static, Message> {
    let caption = if count > 0 {
        format!("{label} ({count})")
    } else {
        label.to_string()
    };
    let button = button(text(caption).size(12))
        .padding([3, 10])
        .style(|_theme, status| secondary_button_style(status));
    if count > 0 {
        button.on_press(message).into()
    } else {
        button.into()
    }
}

fn sftp_date(mtime: Option<u64>) -> String {
    mtime.map(format_mtime).unwrap_or_else(|| String::from("—"))
}

/// Local UTC offset in seconds, cached for the session (timezone is stable).
/// Falls back to 0 (UTC) if it cannot be determined (e.g. the soundness guard
/// on multi-threaded Unix; on Windows it always resolves).
fn local_offset_secs() -> i64 {
    static OFFSET: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| {
        time::UtcOffset::current_local_offset()
            .map(|offset| i64::from(offset.whole_seconds()))
            .unwrap_or(0)
    })
}

/// Format a Unix timestamp as local `YYYY-MM-DD HH:MM`.
fn format_mtime(secs: u64) -> String {
    let local = (secs as i64).saturating_add(local_offset_secs()).max(0) as u64;
    format_epoch_utc(local)
}

/// Format seconds-since-epoch (UTC) as `YYYY-MM-DD HH:MM` using the
/// days-from-civil algorithm (no external date dependency).
fn format_epoch_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let hour = tod / 3600;
    let minute = (tod % 3600) / 60;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

fn sftp_pane_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.0, border_color()),
        ..container::Style::default()
    }
}

/// Wrap a pane in drag plumbing: tracks the cursor while a drag is in flight
/// (so the ghost can follow it) and overlays the ghost when this pane is the one
/// under the pointer.
fn sftp_drag_layer<'a>(
    app: &AditApp,
    pane: SftpPane,
    body: Element<'a, Message>,
) -> Element<'a, Message> {
    let dragging = app.sftp_drag.is_some();
    let content: Element<'a, Message> = match app.sftp_drag_cursor {
        Some(position) if dragging && app.sftp_drag_over == Some(pane) => {
            let (name, count) = drag_subject(app);
            stack![body, drag_ghost(name, count, position)]
                .width(Length::FillPortion(1))
                .height(Fill)
                .into()
        }
        _ => body,
    };
    let mut area = mouse_area(content).on_enter(Message::SftpDragEnter(pane));
    if dragging {
        area = area.on_move(move |point| Message::SftpDragMove(pane, point));
    }
    area.into()
}

/// The dragged file name and how many items the drag carries (the selection if
/// the dragged file is part of a multi-selection, else 1).
fn drag_subject(app: &AditApp) -> (String, usize) {
    match &app.sftp_drag {
        Some((src, name)) => {
            let selection = match src {
                SftpPane::Local => &app.sftp_local_selected,
                SftpPane::Remote => &app.sftp_remote_selected,
            };
            let count = if selection.contains(name) && selection.len() > 1 {
                selection.len()
            } else {
                1
            };
            (name.clone(), count)
        }
        None => (String::new(), 0),
    }
}

/// A floating chip that follows the cursor inside the pane during a drag,
/// positioned with leading spacers (pane-relative coordinates from `on_move`).
fn drag_ghost(name: String, count: usize, position: Point) -> Element<'static, Message> {
    let label = if count > 1 {
        format!("⠿ {name}  +{}", count - 1)
    } else {
        format!("⠿ {name}")
    };
    column![
        Space::new().height(Length::Fixed((position.y + 12.0).max(0.0))),
        row![
            Space::new().width(Length::Fixed((position.x + 14.0).max(0.0))),
            container(text(label).size(11).color(primary_text()))
                .padding([3, 8])
                .style(|_theme| drag_ghost_style()),
        ],
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

fn drag_ghost_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.5, accent()),
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.25),
            offset: Vector::new(0.0, 2.0),
            blur_radius: 8.0,
        },
        ..container::Style::default()
    }
}

/// Pane container that highlights (tinted background + accent border) when it is
/// the active drop target of a pane-to-pane drag.
fn sftp_pane_style_dropzone(active: bool) -> container::Style {
    let mut style = sftp_pane_style();
    if active {
        style.background = Some(Background::Color(accent_soft()));
        style.border = border(RADIUS_SM, 2.0, accent());
    }
    style
}

fn sort_header_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => panel_background_hover(),
        _ => transparent(),
    };
    base_button_style(background, muted_text(), transparent())
}

/// One clickable column header that sorts a pane and shows an arrow when active.
fn sftp_sort_cell(
    label: &'static str,
    pane: SftpPane,
    column: SftpSortKey,
    active: (SftpSortKey, bool),
    width: Length,
) -> Element<'static, Message> {
    let is_active = active.0 == column;
    let arrow = if is_active {
        if active.1 {
            " ▲"
        } else {
            " ▼"
        }
    } else {
        ""
    };
    let color = if is_active { accent() } else { muted_text() };
    button(text(format!("{label}{arrow}")).size(10).color(color))
        .width(width)
        .padding([2, 4])
        .style(|_theme, status| sort_header_button_style(status))
        .on_press(Message::SftpSort(pane, column))
        .into()
}

/// The sortable column header row shared by both panes; the trailing space keeps
/// the columns roughly aligned with the per-row action buttons on the right.
fn sftp_sort_header(pane: SftpPane, active: (SftpSortKey, bool)) -> Element<'static, Message> {
    row![
        sftp_sort_cell("名称", pane, SftpSortKey::Name, active, Length::Fill),
        sftp_sort_cell("大小", pane, SftpSortKey::Size, active, Length::Fixed(64.0)),
        sftp_sort_cell("修改时间", pane, SftpSortKey::Modified, active, Length::Fixed(118.0)),
        Space::new().width(Length::Fixed(132.0)),
    ]
    .spacing(6)
    .padding([0, 6])
    .into()
}

/// Compare two entries by the active sort column/direction (dirs are grouped
/// first by the caller, so this only orders within a group).
fn sftp_cmp(
    key: SftpSortKey,
    ascending: bool,
    a: (&str, u64, Option<u64>),
    b: (&str, u64, Option<u64>),
) -> std::cmp::Ordering {
    let base = match key {
        SftpSortKey::Name => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
        SftpSortKey::Size => a.1.cmp(&b.1),
        SftpSortKey::Modified => a.2.unwrap_or(0).cmp(&b.2.unwrap_or(0)),
    };
    if ascending {
        base
    } else {
        base.reverse()
    }
}

fn sftp_list_inner_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface_alt())),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.0, border_color()),
        ..container::Style::default()
    }
}

fn sftp_entry_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => panel_background_hover(),
        button::Status::Pressed => accent_soft(),
        _ => transparent(),
    };
    base_button_style(background, primary_text(), transparent())
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
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

    profile.group == app.profile_group.trim()
        && profile.name == app.profile_name.trim()
        && profile.host == app.profile_host.trim()
        && profile.port.to_string() == app.profile_port.trim()
        && profile.username == app.profile_username.trim()
        && profile.auth_method == app.profile_auth_method
        && profile.identity_file == app.profile_identity_file.trim()
        && profile.protocol == app.profile_protocol
        && profile.startup_command == app.profile_startup_command.trim()
        && profile.terminal_type == app.profile_terminal_type.trim()
}

fn sidebar(app: &AditApp) -> Element<'_, Message> {
    let mut sorted_profiles = app.manager.profiles().to_vec();
    sorted_profiles.sort_by(profile_sidebar_order);

    let filter = app.session_filter.trim().to_ascii_lowercase();
    let filter_active = !filter.is_empty();
    let profile_count = if filter_active {
        sorted_profiles
            .iter()
            .filter(|profile| profile_matches_filter(profile, &filter))
            .count()
    } else {
        sorted_profiles.len()
    };
    let mut profiles = column![tree_root_row(profile_count)].spacing(1).width(Fill);

    for group in sidebar_group_names(app, &sorted_profiles) {
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
        profiles = profiles.push(tree_group_row(
            group.clone(),
            collapsed,
            group_count,
            group_drop_target,
        ));

        if app.group_context_menu.as_deref() == Some(group.as_str()) {
            profiles = profiles.push(group_context_menu(group.clone(), collapsed));
        }

        if app.editing_group.as_deref() == Some(group.as_str()) {
            profiles = profiles.push(group_edit_menu(app));
        }

        if collapsed {
            continue;
        }

        for profile in group_profiles {
            // The dragged row is "lifted" into the floating ghost, so its slot
            // renders as an empty gap the neighbours squeeze around.
            if Some(profile.id) == app.dragged_profile {
                profiles = profiles.push(profile_drag_gap());
                continue;
            }
            let selected = Some(profile.id) == app.selected_profile;
            let hovered = Some(profile.id) == app.hovered_profile;

            profiles = profiles.push(tree_profile_row(profile, selected, hovered, false));

            // The context menu and the profile editor are now floating overlays
            // (see the layers stack in `view`), not pushed inline here.
        }
    }

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
            row![text("Session Manager").size(13).color(primary_text())]
                .align_y(Alignment::Center),
        )
        .height(Length::Fixed(28.0))
        .padding([3, 10])
        .style(|_theme| sidebar_header_style()),
        row![
            sidebar_tool_button("▶", "连接所选会话", Message::ConnectSelectedProfile),
            sidebar_tool_button("↗", "打开连接对话框", Message::OpenSelectedProfile),
            sidebar_tool_separator(),
            sidebar_tool_button("+", "新建会话", Message::NewProfileDraft),
            sidebar_tool_button("⊞", "新建分组", Message::NewGroupDraft),
            sidebar_tool_button("⤓", "保存会话", Message::SaveProfile),
            sidebar_tool_button("✕", "删除所选", Message::DeleteSelectedProfile),
            sidebar_tool_separator(),
            sidebar_tool_button("↑", "上移", Message::MoveSelectedProfile(ProfileMove::Up)),
            sidebar_tool_button("↓", "下移", Message::MoveSelectedProfile(ProfileMove::Down)),
            sidebar_tool_button("A", "按名称排序", Message::SortProfiles(ProfileSortKey::Name)),
            sidebar_tool_button("H", "按主机排序", Message::SortProfiles(ProfileSortKey::Host)),
            sidebar_tool_separator(),
            sidebar_tool_button("▤", "日志 / 脚本", Message::RunMenu(MenuCommand::Logging)),
            Space::new().width(Fill),
        ]
        .padding([6, 8])
        .spacing(4)
        .align_y(Alignment::Center),
        text_input("Filter by group/session name <Alt+I>", &app.session_filter)
            .on_input(Message::SessionFilterChanged)
            .padding([4, 6])
            .style(toolbar_input_style),
        scrollable(profiles).height(Fill),
    ]
    .spacing(0)
    .height(Fill)
    .width(Length::Fixed(app.sidebar_width));

    if let Some(error) = error {
        content = content.push(error);
    }

    // Track the cursor over the sidebar so a right-click can anchor its floating
    // context menu at the pointer, and (during a drag) so the ghost card follows.
    // `on_move` gives sidebar-relative coordinates.
    let panel = container(content)
        .height(Fill)
        .style(|_theme| sidebar_style());
    let layered: Element<'_, Message> = match (app.dragged_profile, app.profile_drag_cursor) {
        (Some(_), Some(position)) => stack![panel, profile_drag_ghost(app, position)].into(),
        _ => panel.into(),
    };
    mouse_area(layered)
        .on_move(Message::SidebarCursorMoved)
        .into()
}

/// The empty slot left where a dragged profile row used to be — the list
/// squeezes around it and it marks where the drop will land.
fn profile_drag_gap() -> Element<'static, Message> {
    container(Space::new().width(Fill).height(Length::Fixed(PROFILE_ROW_HEIGHT)))
        .width(Fill)
        .style(|_theme| container::Style {
            background: Some(Background::Color(accent_soft())),
            border: border(RADIUS_SM, 1.5, accent()),
            ..container::Style::default()
        })
        .into()
}

/// A floating card that mirrors the dragged profile row and follows the cursor
/// (`position` is sidebar-relative), so the drag reads as picking the row up.
fn profile_drag_ghost(app: &AditApp, position: Point) -> Element<'static, Message> {
    let Some(id) = app.dragged_profile else {
        return Space::new().into();
    };
    let Some(profile) = app.manager.profile(id).cloned() else {
        return Space::new().into();
    };
    let endpoint = if profile.username.trim().is_empty() {
        profile.host.clone()
    } else {
        format!("{}@{}", profile.username, profile.host)
    };

    let card = container(
        row![
            Space::new().width(Length::Fixed(4.0)),
            avatar(&profile.name),
            column![
                text(profile.name.clone()).size(12).color(primary_text()),
                text(endpoint).size(10).color(muted_text()),
            ]
            .spacing(0),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .height(Length::Fixed(PROFILE_ROW_HEIGHT))
    .width(Length::Fixed((app.sidebar_width - 28.0).max(140.0)))
    .padding([2, 8])
    .style(|_theme| profile_drag_ghost_style());

    // Carry the card under the cursor: centered on it vertically, nudged right.
    let top = (position.y - PROFILE_ROW_HEIGHT / 2.0).max(0.0);
    let left = (position.x - 18.0).max(0.0);
    column![
        Space::new().height(Length::Fixed(top)),
        row![Space::new().width(Length::Fixed(left)), card],
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

fn profile_drag_ghost_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.5, accent()),
        shadow: soft_shadow(),
        ..container::Style::default()
    }
}

/// The draggable divider between the sidebar and the workspace. Pressing it
/// starts a resize drag; the drag itself is driven by the global cursor
/// subscription that is only active while `sidebar_dragging` is set.
fn sidebar_divider() -> Element<'static, Message> {
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

fn tree_root_row(profile_count: usize) -> Element<'static, Message> {
    container(
        row![
            text("▾").size(11).color(muted_text()),
            text("Hosts").size(12).color(primary_text()),
            Space::new().width(Fill),
            text(profile_count.to_string()).size(11).color(muted_text()),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding([6, 6])
    .width(Fill)
    .into()
}

fn sidebar_group_names(app: &AditApp, profiles: &[ConnectionProfile]) -> Vec<String> {
    let mut groups = app.groups.clone();
    groups.extend(groups_from_profiles(profiles));

    if groups.is_empty() {
        groups.insert(String::from("Default"));
    }

    groups.into_iter().collect()
}

fn tree_group_row(
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
                Space::new().width(Length::Fixed(10.0)),
                text(arrow).size(11).color(muted_text()),
                text(group_label).size(12).color(muted_text()),
                Space::new().width(Fill),
                text(profile_count.to_string()).size(10).color(muted_text()),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding([6, 6])
        .width(Fill)
        .style(move |_theme| group_row_style(drop_target)),
    )
    .on_press(Message::ToggleProfileGroup(toggle_group))
    .on_right_press(Message::ShowGroupContextMenu(context_group))
    .on_enter(Message::ProfileDragOverGroup(enter_group))
    .on_move(move |_| Message::ProfileDragOverGroup(hover_group.clone()))
    .on_release(Message::ProfileDroppedOnGroup(release_group))
    .on_exit(Message::ProfileGroupHoverExited(exit_group))
    .interaction(mouse::Interaction::Pointer)
    .into()
}

fn group_context_menu(group: String, collapsed: bool) -> Element<'static, Message> {
    let toggle_label = if collapsed { "展开" } else { "折叠" };
    container(
        row![
            Space::new().width(Length::Fixed(42.0)),
            profile_context_button("重命名", Message::RenameGroupFromContext(group.clone())),
            profile_context_button("新会话", Message::NewProfileInGroup(group.clone())),
            profile_context_button("删空组", Message::DeleteGroupFromContext(group.clone())),
            profile_context_button(toggle_label, Message::ToggleProfileGroup(group)),
            profile_context_button("关闭", Message::HideGroupContextMenu),
        ]
        .spacing(3)
        .align_y(Alignment::Center),
    )
    .padding([3, 4])
    .width(Fill)
    .style(|_theme| profile_context_menu_style())
    .into()
}

fn group_edit_menu(app: &AditApp) -> Element<'_, Message> {
    container(
        row![
            Space::new().width(Length::Fixed(42.0)),
            text_input("Group name", &app.group_name_draft)
                .on_input(Message::GroupNameDraftChanged)
                .on_submit(Message::SaveGroupRename)
                .padding([4, 6])
                .style(text_input_style)
                .width(Fill),
            button("保存")
                .padding([4, 8])
                .style(|_theme, status| primary_button_style(status))
                .on_press(Message::SaveGroupRename),
            button("取消")
                .padding([4, 8])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::CancelGroupRename),
        ]
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .padding([4, 6])
    .width(Fill)
    .style(|_theme| profile_edit_menu_style())
    .into()
}

fn tree_profile_row(
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

    let endpoint = if profile.username.trim().is_empty() {
        profile.host.clone()
    } else {
        format!("{}@{}", profile.username, profile.host)
    };

    mouse_area(
        container(
            row![
                Space::new().width(Length::Fixed(4.0)),
                avatar(&profile.name),
                column![
                    text(profile.name.clone()).size(12).color(primary_text()),
                    text(endpoint).size(10).color(muted_text()),
                ]
                .spacing(0),
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

fn profile_drop_position(point: Point) -> ProfileDropPosition {
    if point.y < PROFILE_ROW_HEIGHT / 2.0 {
        ProfileDropPosition::Before
    } else {
        ProfileDropPosition::After
    }
}

/// A Termius-style round host avatar: the host's initials on a per-host color.
fn avatar(name: &str) -> Element<'static, Message> {
    let color = avatar_color(name);
    container(text(avatar_initials(name)).size(10).color(Color::WHITE))
        .center_x(Length::Fixed(24.0))
        .center_y(Length::Fixed(24.0))
        .style(move |_theme| avatar_style(color))
        .into()
}

fn avatar_initials(name: &str) -> String {
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

fn avatar_color(name: &str) -> Color {
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

fn avatar_style(color: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(color)),
        text_color: Some(Color::WHITE),
        border: border(16.0, 0.0, transparent()),
        ..container::Style::default()
    }
}

const PROFILE_MENU_WIDTH: f32 = 168.0;
const PROFILE_MENU_HEIGHT: f32 = 132.0;

/// The context-menu card (used inside the floating overlay).
fn profile_context_menu(profile_id: ProfileId) -> Element<'static, Message> {
    container(
        column![
            profile_menu_item("连接", Message::ConnectProfileFromContext(profile_id), false),
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
fn profile_context_overlay(app: &AditApp, profile_id: ProfileId) -> Element<'_, Message> {
    floating_context_menu(
        app,
        profile_context_menu(profile_id),
        Message::HideProfileContextMenu,
    )
}

fn terminal_context_overlay(app: &AditApp) -> Element<'_, Message> {
    floating_context_menu(
        app,
        terminal_context_menu(),
        Message::HideTerminalContextMenu,
    )
}

/// Place a context-menu `card` at the last-tracked cursor position (clamped to
/// the window) over a scrim that dismisses it on any outside click.
fn floating_context_menu<'a>(
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

fn profile_menu_item(label: &'static str, message: Message, danger: bool) -> Element<'static, Message> {
    button(text(label).size(12))
        .width(Fill)
        .padding([6, 10])
        .style(move |_theme, status| profile_menu_item_style(status, danger))
        .on_press(message)
        .into()
}

fn profile_menu_divider() -> Element<'static, Message> {
    container(Space::new().height(Length::Fixed(1.0)))
        .width(Fill)
        .style(|_theme| container::Style {
            background: Some(Background::Color(border_color())),
            ..container::Style::default()
        })
        .into()
}

fn profile_context_button(label: &'static str, message: Message) -> Element<'static, Message> {
    button(text(label).size(11))
        .padding([3, 7])
        .style(|_theme, status| profile_context_button_style(status))
        .on_press(message)
        .into()
}

fn profile_matches_filter(profile: &ConnectionProfile, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }

    profile.group.to_ascii_lowercase().contains(filter)
        || profile.name.to_ascii_lowercase().contains(filter)
        || profile.host.to_ascii_lowercase().contains(filter)
        || profile.username.to_ascii_lowercase().contains(filter)
        || profile.endpoint().to_ascii_lowercase().contains(filter)
}

fn profile_sidebar_order(
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

fn sidebar_tool_button(
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

fn sidebar_tool_separator() -> Element<'static, Message> {
    container(Space::new().height(Length::Fixed(16.0)))
        .width(Length::Fixed(1.0))
        .style(|_theme| container::Style {
            background: Some(Background::Color(border_color())),
            ..container::Style::default()
        })
        .into()
}

fn tooltip_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.0, border_color()),
        shadow: subtle_shadow(),
        ..container::Style::default()
    }
}

fn dialog_field<'a>(label: &'static str, input: Element<'a, Message>) -> Element<'a, Message> {
    column![text(label).size(11).color(muted_text()), input]
        .spacing(3)
        .into()
}

/// The session editor as a centered modal dialog (over a scrim), instead of an
/// inline editor embedded in the sidebar list.
fn profile_editor_overlay(app: &AditApp) -> Element<'_, Message> {
    let status = if form_matches_selected_profile(app) {
        "已保存"
    } else {
        "未保存"
    };

    let header = row![
        text("编辑会话").size(15).color(primary_text()),
        text(status).size(11).color(muted_text()),
        Space::new().width(Fill),
        button("×")
            .width(Length::Fixed(26.0))
            .height(Length::Fixed(24.0))
            .padding(0)
            .style(|_theme, status| close_button_style(status))
            .on_press(Message::CloseProfileEditor),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut form = column![
        header,
        dialog_field(
            "协议",
            row![
                protocol_button(app, Protocol::Ssh),
                protocol_button(app, Protocol::LocalShell),
                protocol_button(app, Protocol::Serial),
                protocol_button(app, Protocol::Rdp),
            ]
            .spacing(6)
            .into(),
        ),
        row![
            dialog_field(
                "分组",
                text_input("默认", &app.profile_group)
                    .on_input(Message::ProfileGroupChanged)
                    .padding([5, 8])
                    .style(text_input_style)
                    .width(Fill)
                    .into(),
            ),
            dialog_field(
                "名称",
                text_input("会话名称", &app.profile_name)
                    .on_input(Message::ProfileNameChanged)
                    .padding([5, 8])
                    .style(text_input_style)
                    .width(Fill)
                    .into(),
            ),
        ]
        .spacing(10),
    ]
    .spacing(12);

    match app.profile_protocol {
        Protocol::Ssh => {
            form = form
                .push(
                    row![
                        container(dialog_field(
                            "主机",
                            text_input("10.0.0.5", &app.profile_host)
                                .on_input(Message::ProfileHostChanged)
                                .on_submit(Message::ConnectSelectedProfile)
                                .padding([5, 8])
                                .style(text_input_style)
                                .width(Fill)
                                .into(),
                        ))
                        .width(Length::FillPortion(2)),
                        container(dialog_field(
                            "端口",
                            text_input("22", &app.profile_port)
                                .on_input(Message::ProfilePortChanged)
                                .padding([5, 8])
                                .style(text_input_style)
                                .width(Fill)
                                .into(),
                        ))
                        .width(Length::FillPortion(1)),
                    ]
                    .spacing(10),
                )
                .push(dialog_field(
                    "用户名",
                    text_input("root", &app.profile_username)
                        .on_input(Message::ProfileUsernameChanged)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ))
                .push(dialog_field(
                    "认证方式",
                    row![
                        auth_method_button(app, AuthMethod::Auto),
                        auth_method_button(app, AuthMethod::Password),
                        auth_method_button(app, AuthMethod::Key),
                        auth_method_button(app, AuthMethod::Agent),
                    ]
                    .spacing(6)
                    .into(),
                ))
                .push(dialog_field(
                    "密钥文件（可选）",
                    row![
                        text_input("~/.ssh/id_ed25519", &app.profile_identity_file)
                            .on_input(Message::ProfileIdentityFileChanged)
                            .padding([5, 8])
                            .style(text_input_style)
                            .width(Fill),
                        button(text("浏览…").size(12))
                            .padding([5, 12])
                            .style(|_theme, status| secondary_button_style(status))
                            .on_press(Message::PickIdentityFile),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center)
                    .into(),
                ))
                .push(dialog_field(
                    "启动命令（可选，连接后自动执行，如 tmux attach）",
                    text_input("tmux new -A -s main", &app.profile_startup_command)
                        .on_input(Message::ProfileStartupCommandChanged)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ))
                .push(dialog_field(
                    "终端类型 TERM（可选，默认 xterm-256color）",
                    text_input("xterm-256color", &app.profile_terminal_type)
                        .on_input(Message::ProfileTerminalTypeChanged)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ));
        }
        Protocol::LocalShell => {
            form = form.push(dialog_field(
                "Shell 程序（可选，留空用系统默认）",
                text_input("powershell.exe / cmd.exe / bash", &app.profile_identity_file)
                    .on_input(Message::ProfileIdentityFileChanged)
                    .on_submit(Message::ConnectSelectedProfile)
                    .padding([5, 8])
                    .style(text_input_style)
                    .width(Fill)
                    .into(),
            ));
        }
        Protocol::Serial => {
            form = form
                .push(dialog_field(
                    "串口号",
                    text_input("COM3", &app.profile_host)
                        .on_input(Message::ProfileHostChanged)
                        .on_submit(Message::ConnectSelectedProfile)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ))
                .push(dialog_field(
                    "波特率（8N1，无流控）",
                    text_input("115200", &app.profile_identity_file)
                        .on_input(Message::ProfileIdentityFileChanged)
                        .on_submit(Message::ConnectSelectedProfile)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ));
        }
        Protocol::Rdp => {
            form = form
                .push(
                    row![
                        container(dialog_field(
                            "主机",
                            text_input("10.0.0.5", &app.profile_host)
                                .on_input(Message::ProfileHostChanged)
                                .on_submit(Message::ConnectSelectedProfile)
                                .padding([5, 8])
                                .style(text_input_style)
                                .width(Fill)
                                .into(),
                        ))
                        .width(Length::FillPortion(2)),
                        container(dialog_field(
                            "端口",
                            text_input("3389", &app.profile_port)
                                .on_input(Message::ProfilePortChanged)
                                .padding([5, 8])
                                .style(text_input_style)
                                .width(Fill)
                                .into(),
                        ))
                        .width(Length::FillPortion(1)),
                    ]
                    .spacing(10),
                )
                .push(dialog_field(
                    "用户名",
                    text_input("Administrator", &app.profile_username)
                        .on_input(Message::ProfileUsernameChanged)
                        .padding([5, 8])
                        .style(text_input_style)
                        .width(Fill)
                        .into(),
                ))
                .push(
                    text("连接时调起系统远程桌面 (mstsc)；密码在 mstsc 中输入。")
                        .size(11)
                        .color(muted_text()),
                );
        }
    }

    form = form.push(
        row![
            Space::new().width(Fill),
            button(text("取消").size(12))
                .padding([6, 16])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::CloseProfileEditor),
            button(text("连接").size(12))
                .padding([6, 16])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::ConnectSelectedProfile),
            button(text("保存").size(12))
                .padding([6, 18])
                .style(|_theme, status| primary_button_style(status))
                .on_press(Message::SaveProfile),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    );

    let card = container(form)
        .width(Length::Fixed(440.0))
        .padding(20)
        .style(|_theme| connection_dialog_style());

    container(card)
        .width(Fill)
        .height(Fill)
        .center_x(Fill)
        .center_y(Fill)
        .style(|_theme| dialog_scrim_style())
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

fn protocol_button(app: &AditApp, protocol: Protocol) -> Element<'static, Message> {
    let selected = app.profile_protocol == protocol;

    button(text(protocol.label()).size(11))
        .padding([4, 10])
        .style(move |_theme, status| method_button_style(selected, status))
        .on_press(Message::ProfileProtocolChanged(protocol))
        .into()
}

fn workspace(app: &AditApp) -> Element<'_, Message> {
    let tabs = app
        .manager
        .sessions()
        .into_iter()
        .fold(row![].spacing(2).height(TAB_BAR_HEIGHT), |tabs, session| {
            tabs.push(tab_button(
                session,
                app.manager.active_session(),
                app.dragged_tab,
            ))
        });

    // Split panes: 2–4 tiled sessions. Otherwise the single-pane view, left
    // byte-for-byte as before (it is the well-tested selection/hit-test path).
    let body: Element<'_, Message> = if app.panes.len() >= 2 {
        tiled_workspace_body(app)
    } else {
        let snapshot = active_terminal_snapshot(app);
        let highlights = search_highlights_for(app, &snapshot);
        mouse_area(terminal_view(
            snapshot,
            app.terminal_focused,
            app.terminal_selection,
            app.terminal_scroll_offset,
            highlights,
        ))
        .on_press(Message::BeginTerminalSelection)
        .on_release(Message::EndTerminalSelection)
        .on_right_press(Message::ShowTerminalContextMenu)
        .on_move(Message::TerminalPointerMoved)
        .on_scroll(Message::TerminalScrolled)
        .interaction(mouse::Interaction::Text)
        .into()
    };

    let tab_row = row![
        scrollable(tabs).direction(scrollable::Direction::Horizontal(
            scrollable::Scrollbar::new()
        )),
        active_session_action(app),
        container(text(app.manager.status_line()).size(12).color(muted_text()))
            .padding([0, 8])
            .center_y(TAB_BAR_HEIGHT),
        Space::new().width(Fill),
        split_button(app),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .height(TAB_BAR_HEIGHT)
    .width(Fill);

    let mut layout = column![tab_row].height(Fill).width(Fill);
    if app.search_open {
        layout = layout.push(terminal_search_bar(app));
    }
    layout = layout.push(body);
    if app.command_window_open {
        layout = layout.push(command_window_bar(app));
    }

    container(layout)
        .padding(0)
        .style(|_theme| workspace_style())
        .height(Fill)
        .width(Fill)
        .into()
}

/// The scrollback-search bar shown above the terminal (Ctrl+Shift+F).
fn terminal_search_bar(app: &AditApp) -> Element<'_, Message> {
    let count = app.search_matches.len();
    let status = if app.search_query.is_empty() {
        String::new()
    } else if count == 0 {
        String::from("无匹配")
    } else {
        format!("{}/{}", app.search_index.map(|i| i + 1).unwrap_or(0), count)
    };

    container(
        row![
            text("查找").size(12).color(muted_text()),
            text_input("搜索终端历史…", &app.search_query)
                .id(search_input_id())
                .on_input(Message::SearchQueryChanged)
                .on_submit(Message::SearchNext)
                .padding([4, 8])
                .style(text_input_style)
                .width(Length::Fixed(280.0)),
            container(text(status).size(11).color(muted_text()))
                .width(Length::Fixed(64.0)),
            button(text("↑").size(13))
                .padding([3, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::SearchPrev),
            button(text("↓").size(13))
                .padding([3, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::SearchNext),
            Space::new().width(Fill),
            button(text("×").size(14))
                .padding([3, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::CloseSearch),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding([4, 8])
    .width(Fill)
    .style(|_theme| toolbar_style())
    .into()
}

/// The bottom command window: type a line and send it to the active session or
/// broadcast it to every session, SecureCRT-style. The text lives in
/// `terminal_input`; sending / history / send-immediately are handled here.
fn command_window_bar(app: &AditApp) -> Element<'_, Message> {
    let target = app.command_target;
    let broadcasting = target == CommandTarget::AllSessions;
    let target_label = if broadcasting {
        format!("→ 所有会话 ({})", app.manager.live_session_count())
    } else {
        format!("→ {}", target.label())
    };

    let placeholder = if app.command_send_immediately {
        "逐字符即时发送到目标…（回车提交整行）"
    } else if broadcasting {
        "输入命令，回车广播到所有会话"
    } else {
        "输入命令，回车发送到当前会话"
    };

    let immediate = app.command_send_immediately;

    container(
        row![
            button(text(target_label).size(12))
                .padding([4, 10])
                .style(move |_theme, status| if broadcasting {
                    base_button_style(accent(), Color::from_rgb8(245, 249, 255), transparent())
                } else {
                    secondary_button_style(status)
                })
                .on_press(Message::CommandTargetToggled),
            text_input(placeholder, &app.terminal_input)
                .id(command_input_id())
                .on_input(Message::TerminalInputChanged)
                .on_submit(Message::SendTerminalInput)
                .padding([4, 8])
                .style(text_input_style)
                .width(Fill),
            button(text("▲").size(11))
                .padding([3, 8])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::CommandHistoryPrev),
            button(text("▼").size(11))
                .padding([3, 8])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::CommandHistoryNext),
            button(text("即时").size(12))
                .padding([4, 10])
                .style(move |_theme, status| if immediate {
                    base_button_style(accent(), Color::from_rgb8(245, 249, 255), transparent())
                } else {
                    secondary_button_style(status)
                })
                .on_press(Message::ToggleCommandSendImmediately),
            button(text("发送").size(12))
                .padding([4, 14])
                .style(|_theme, status| primary_button_style(status))
                .on_press(Message::SendTerminalInput),
            button(text("×").size(14))
                .padding([3, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::ToggleCommandWindow),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding([4, 8])
    .width(Fill)
    .style(|_theme| toolbar_style())
    .into()
}

/// The tab-row split control: adds another connected session as a pane.
fn split_button(app: &AditApp) -> Element<'static, Message> {
    let label = if app.panes.len() >= 2 {
        format!("▥ 分屏 {}", app.panes.len())
    } else {
        String::from("▥ 分屏")
    };
    button(text(label).size(11))
        .padding([3, 10])
        .style(|_theme, status| secondary_button_style(status))
        .on_press(Message::SplitPane)
        .into()
}

/// Tile the current `panes` into a row/grid, each a headed terminal pane.
fn tiled_workspace_body(app: &AditApp) -> Element<'_, Message> {
    let layout = pane_layout(app);
    let mut grid = column![].spacing(PANE_GAP).width(Fill).height(Fill);
    let mut idx = 0usize;

    while idx < app.panes.len() {
        let mut r = row![].spacing(PANE_GAP).width(Fill).height(Fill);
        for _ in 0..layout.cols {
            if idx >= app.panes.len() {
                break;
            }
            let session_id = app.panes[idx];
            r = r.push(
                container(terminal_pane(app, session_id, idx))
                    .width(Length::FillPortion(1))
                    .height(Fill),
            );
            idx += 1;
        }
        grid = grid.push(r);
    }

    grid.into()
}

/// One split pane: a clickable header (session title + close-pane ×) over a
/// terminal body wired to pane-scoped input/selection messages.
fn terminal_pane(app: &AditApp, session_id: SessionId, index: usize) -> Element<'static, Message> {
    let is_focused = index == app.focused_pane;
    let summary = app.manager.session_summary(session_id);
    let title = summary
        .as_ref()
        .map(|summary| summary.title.clone())
        .unwrap_or_else(|| String::from("会话"));
    let status = summary
        .map(|summary| summary.status)
        .unwrap_or(SessionStatus::Disconnected);

    let header = mouse_area(
        container(
            row![
                text("●").size(9).color(status_color(status)),
                text(title).size(11).color(primary_text()).width(Fill),
                button(text("×").size(13))
                    .padding([0, 6])
                    .style(|_theme, status| tab_close_button_style(status))
                    .on_press(Message::ClosePane(index)),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding([1, 6])
        .height(Length::Fixed(PANE_HEADER_HEIGHT))
        .width(Fill)
        .style(move |_theme| pane_header_style(is_focused)),
    )
    .on_press(Message::FocusPane(index))
    .interaction(mouse::Interaction::Pointer);

    let snapshot = pane_snapshot(app, session_id, is_focused);
    let selection = if is_focused {
        app.terminal_selection
    } else {
        None
    };
    let highlights = if is_focused {
        search_highlights_for(app, &snapshot)
    } else {
        Vec::new()
    };
    let body = mouse_area(terminal_view(
        snapshot,
        is_focused,
        selection,
        app.terminal_scroll_offset,
        highlights,
    ))
    .on_press(Message::PaneMousePressed(index))
    .on_release(Message::EndTerminalSelection)
    .on_right_press(Message::PaneRightPressed(index))
    .on_move(move |point| Message::PanePointerMoved(index, point))
    .on_scroll(Message::TerminalScrolled)
    .interaction(mouse::Interaction::Text);

    column![header, body]
        .spacing(0)
        .width(Fill)
        .height(Fill)
        .into()
}

/// Snapshot for a pane; only the focused pane honors the scroll-back offset.
fn pane_snapshot(app: &AditApp, session_id: SessionId, is_focused: bool) -> TerminalSnapshot {
    let rows = terminal_view_rows(app);
    let tail = app.manager.snapshot_for(session_id, Viewport::tail(rows));

    if !is_focused || app.terminal_scroll_offset == 0 {
        return tail;
    }

    let offset = app
        .terminal_scroll_offset
        .min(max_scroll_offset_for(&tail, rows));
    let first_row = tail.total_rows.saturating_sub(rows).saturating_sub(offset);
    app.manager.snapshot_for(
        session_id,
        Viewport {
            first_row,
            height: rows,
        },
    )
}

fn active_session_action(app: &AditApp) -> Element<'_, Message> {
    if app.manager.active_session_summary().is_some_and(|summary| {
        matches!(
            summary.status,
            SessionStatus::Error | SessionStatus::Disconnected
        )
    }) {
        return button(text("重连").size(12))
            .padding([4, 10])
            .style(|_theme, status| primary_button_style(status))
            .on_press(Message::RetryActiveSession)
            .into();
    }

    Space::new().width(Length::Shrink).into()
}

fn tab_button(
    session: SessionSummary,
    active_session: Option<SessionId>,
    dragged: Option<SessionId>,
) -> Element<'static, Message> {
    let id = session.id;
    let active = Some(id) == active_session;
    // The tab currently being dragged gets a "lifted" accent so its live
    // reordering is easy to follow.
    let is_dragging = dragged == Some(id);

    // The whole pill is a mouse_area (click = activate, drag = reorder); only the
    // close × stays a button so it can consume its own click.
    let inner = row![
        text("●").size(10).color(status_color(session.status)),
        text(session.title).size(12).color(primary_text()),
        button(text("×").size(15))
            .padding([2, 7])
            .style(|_theme, status| tab_close_button_style(status))
            .on_press(Message::CloseSession(id)),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    mouse_area(
        container(inner)
            .padding([2, 6])
            .style(move |_theme| tab_container_style_dnd(active, is_dragging)),
    )
    .on_press(Message::TabPressed(id))
    .on_release(Message::TabReleased)
    .on_enter(Message::TabDragOver(id))
    .on_right_press(Message::RenameSessionPrompt(id))
    .interaction(mouse::Interaction::Pointer)
    .into()
}

fn terminal_view(
    snapshot: TerminalSnapshot,
    focused: bool,
    selection: Option<TerminalSelection>,
    _scroll_offset: usize,
    search_highlights: Vec<Vec<(usize, usize, bool)>>,
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
                let highlights = search_highlights
                    .get(row_index)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                column.push(terminal_line(line, row_index, selection, highlights))
            })
    };

    // The context menu now floats (see the layers stack in `view`), so the
    // terminal body no longer reserves a strip for it.
    container(container(lines).height(Fill).width(Fill))
        .padding(TERMINAL_PANEL_PADDING as u16)
        .height(Fill)
        .width(Fill)
        .style(move |_theme| terminal_panel_style(focused))
        .into()
}

/// The terminal context-menu card (used inside the floating overlay).
fn terminal_context_menu() -> Element<'static, Message> {
    container(
        column![
            profile_menu_item("复制", Message::CopyTerminalSelection, false),
            profile_menu_item("粘贴", Message::PasteIntoTerminal, false),
            profile_menu_divider(),
            profile_menu_item("清屏", Message::ClearActiveTerminal, false),
            profile_menu_item("回到底部", Message::TerminalJumpToBottom, false),
        ]
        .spacing(1),
    )
    .padding(4)
    .width(Length::Fixed(PROFILE_MENU_WIDTH))
    .style(|_theme| profile_context_menu_style())
    .into()
}

fn terminal_line(
    line: TerminalLine,
    row_index: usize,
    selection: Option<TerminalSelection>,
    search: &[(usize, usize, bool)],
) -> Element<'static, Message> {
    let font_size = term_font_size();
    let base_font = term_font();
    let cell_w = cell_width();
    let cell_h = cell_height();

    if line.cells.is_empty() {
        // Preserve the exact row height of a visually blank terminal line.
        return container(text(" ").size(font_size).font(base_font))
            .height(Length::Fixed(cell_h))
            .into();
    }

    let selected_range =
        selection.and_then(|selection| selection_range_for_row(selection, row_index));
    let selected_fg = selection_foreground();
    let mut col = 0_usize;
    let mut row_widget = row![].spacing(0);

    for cell in line.cells {
        let mut fg = term_color(cell.fg, default_foreground());
        if cell.dim {
            fg = dim_color(fg);
        }
        let font = Font {
            weight: if cell.bold {
                Weight::Bold
            } else {
                Weight::Normal
            },
            style: if cell.italic {
                iced::font::Style::Italic
            } else {
                iced::font::Style::Normal
            },
            ..base_font
        };

        for ch in cell.text.chars() {
            let selected = selected_range.is_some_and(|range| col >= range.0 && col < range.1);
            let search_hit = search
                .iter()
                .find_map(|(start, end, current)| (col >= *start && col < *end).then_some(*current));

            let glyph_color = if selected {
                selected_fg
            } else if let Some(current) = search_hit {
                if current {
                    Color::from_rgb8(24, 24, 24)
                } else {
                    Color::from_rgb8(245, 236, 210)
                }
            } else {
                fg
            };
            let label = text(ch.to_string())
                .size(font_size)
                .font(font)
                .color(glyph_color);

            let background = if selected {
                Some(selection_background())
            } else if let Some(current) = search_hit {
                Some(if current {
                    Color::from_rgb8(240, 180, 60)
                } else {
                    Color::from_rgb8(96, 82, 44)
                })
            } else {
                match cell.bg {
                    TermColor::Default => None,
                    other => Some(term_color(other, default_foreground())),
                }
            };

            // Fixed-size cell so the rendered grid exactly matches the
            // pixel→cell hit-testing used for selection (no drift).
            row_widget = row_widget.push(
                container(label)
                    .width(Length::Fixed(cell_w))
                    .height(Length::Fixed(cell_h))
                    .style(move |_theme| container::Style {
                        background: background.map(Background::Color),
                        ..container::Style::default()
                    }),
            );

            col += 1;
        }
    }

    row_widget.into()
}

/// Dim (SGR 2) foreground: scale the glyph color toward black so faint text
/// reads as fainter than normal.
fn dim_color(color: Color) -> Color {
    Color {
        r: color.r * 0.6,
        g: color.g * 0.6,
        b: color.b * 0.6,
        a: color.a,
    }
}

/// Text color for selected cells: dark on a light selection highlight, light on
/// a dark one, so selected glyphs stay legible across every scheme.
fn selection_foreground() -> Color {
    let (r, g, b) = active_scheme().selection;
    let luminance = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if luminance > 140.0 {
        Color::from_rgb8(20, 22, 28)
    } else {
        Color::from_rgb8(245, 249, 255)
    }
}

fn default_foreground() -> Color {
    let (r, g, b) = active_scheme().foreground;
    Color::from_rgb8(r, g, b)
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
    match index {
        0..=15 => {
            let (r, g, b) = active_scheme().ansi[index as usize];
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

    // Left cluster: a red REC badge while the active session is logging,
    // followed by the current status/notice text.
    let mut left = row![].spacing(7).align_y(Alignment::Center);
    if app.manager.active_is_logging() {
        left = left
            .push(text("●").size(11).color(danger()))
            .push(text("REC").size(11).color(danger()));
    }
    if app.broadcast_input {
        // Always-visible warning that keystrokes fan out to every session.
        let reach = app.manager.live_session_count();
        left = left
            .push(text("⇶").size(12).color(accent()))
            .push(text(format!("广播 ×{reach}")).size(11).color(accent()));
    }
    left = left.push(text(status).size(12).color(muted_text()));

    container(
        row![
            left,
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
    .height(STATUS_BAR_HEIGHT)
    .width(Fill)
    .style(|_theme| status_bar_style())
    .into()
}

// Corner-radius scale. Interactive controls and floating surfaces are rounded;
// full-bleed structural bars stay square (see the *_style fns below).
const RADIUS_SM: f32 = 8.0;
const RADIUS_MD: f32 = 12.0;

/// Resolve a token to its light or dark value based on the active mode.
fn pick(light: Color, dark: Color) -> Color {
    if is_dark() {
        dark
    } else {
        light
    }
}

// Palette inspired by Termius: deep navy-charcoal dark chrome, a clean light
// theme, and a teal-green accent. Tokens resolve to a (light, dark) pair.
fn muted_text() -> Color {
    pick(Color::from_rgb8(108, 113, 134), Color::from_rgb8(139, 144, 160))
}

fn primary_text() -> Color {
    pick(Color::from_rgb8(28, 34, 48), Color::from_rgb8(230, 232, 238))
}

fn app_background() -> Color {
    pick(Color::from_rgb8(244, 246, 249), Color::from_rgb8(21, 22, 30))
}

/// Raised surface: sidebar, cards, dialogs, floating menus.
fn surface() -> Color {
    pick(Color::from_rgb8(255, 255, 255), Color::from_rgb8(27, 29, 41))
}

/// Secondary chrome: toolbar, status bar, tab strip.
fn surface_alt() -> Color {
    pick(Color::from_rgb8(238, 241, 246), Color::from_rgb8(27, 29, 41))
}

/// Recessed area the terminal panel floats on.
fn surface_sunken() -> Color {
    pick(Color::from_rgb8(231, 235, 241), Color::from_rgb8(18, 18, 25))
}

fn panel_background_hover() -> Color {
    pick(Color::from_rgb8(232, 242, 240), Color::from_rgb8(38, 42, 56))
}

fn field_background() -> Color {
    pick(Color::from_rgb8(255, 255, 255), Color::from_rgb8(32, 35, 47))
}

fn terminal_background() -> Color {
    let (r, g, b) = active_scheme().background;
    Color::from_rgb8(r, g, b)
}

fn selection_background() -> Color {
    let (r, g, b) = active_scheme().selection;
    Color::from_rgb8(r, g, b)
}

fn border_color() -> Color {
    pick(Color::from_rgb8(225, 230, 236), Color::from_rgb8(38, 42, 56))
}

fn border_strong() -> Color {
    pick(Color::from_rgb8(157, 217, 208), Color::from_rgb8(54, 64, 85))
}

fn accent() -> Color {
    // Deep enough that white button text stays legible.
    Color::from_rgb8(15, 158, 140)
}

fn accent_hover() -> Color {
    Color::from_rgb8(22, 182, 164)
}

fn accent_pressed() -> Color {
    Color::from_rgb8(11, 124, 110)
}

/// Soft accent tint for selected/active backgrounds.
fn accent_soft() -> Color {
    pick(Color::from_rgb8(220, 242, 238), Color::from_rgb8(26, 48, 43))
}

fn danger() -> Color {
    Color::from_rgb8(229, 72, 77)
}

fn danger_background() -> Color {
    pick(Color::from_rgb8(253, 237, 237), Color::from_rgb8(58, 36, 38))
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

/// Pronounced elevation for modal dialogs.
fn soft_shadow() -> Shadow {
    Shadow {
        color: Color {
            r: 0.05,
            g: 0.09,
            b: 0.16,
            a: 0.18,
        },
        offset: Vector::new(0.0, 10.0),
        blur_radius: 28.0,
    }
}

/// Light elevation for dropdowns and context menus.
fn subtle_shadow() -> Shadow {
    Shadow {
        color: Color {
            r: 0.05,
            g: 0.09,
            b: 0.16,
            a: 0.12,
        },
        offset: Vector::new(0.0, 4.0),
        blur_radius: 14.0,
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
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(0.0, 1.0, border_color()),
        ..container::Style::default()
    }
}

fn menu_dropdown_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.0, border_color()),
        shadow: subtle_shadow(),
        ..container::Style::default()
    }
}

fn toolbar_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface_alt())),
        text_color: Some(primary_text()),
        border: border(0.0, 1.0, border_color()),
        ..container::Style::default()
    }
}

fn sidebar_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(0.0, 1.0, border_color()),
        ..container::Style::default()
    }
}

fn workspace_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface_sunken())),
        text_color: Some(primary_text()),
        ..container::Style::default()
    }
}

fn terminal_panel_style(focused: bool) -> container::Style {
    container::Style {
        background: Some(Background::Color(terminal_background())),
        text_color: Some(default_foreground()),
        border: border(
            RADIUS_MD,
            1.5,
            if focused {
                accent()
            } else {
                Color::from_rgb8(38, 43, 54)
            },
        ),
        ..container::Style::default()
    }
}

/// The title bar of a split pane; accent-tinted while it is the focused pane.
fn pane_header_style(focused: bool) -> container::Style {
    container::Style {
        background: Some(Background::Color(if focused {
            accent_soft()
        } else {
            surface_alt()
        })),
        text_color: Some(primary_text()),
        border: border(
            RADIUS_SM,
            1.0,
            if focused { accent() } else { border_color() },
        ),
        ..container::Style::default()
    }
}

fn dialog_scrim_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color {
            r: 0.04,
            g: 0.06,
            b: 0.10,
            a: 0.42,
        })),
        ..container::Style::default()
    }
}

fn connection_dialog_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(RADIUS_MD, 1.0, border_color()),
        shadow: soft_shadow(),
        ..container::Style::default()
    }
}

fn status_bar_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface_alt())),
        text_color: Some(muted_text()),
        border: border(0.0, 1.0, border_color()),
        ..container::Style::default()
    }
}

fn error_panel_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(danger_background())),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.0, danger()),
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
        border: border(RADIUS_SM, 1.0, border_color),
        icon: muted_text(),
        placeholder: muted_text(),
        value: primary_text(),
        selection: accent_soft(),
    }
}

fn base_button_style(background: Color, text_color: Color, border_color: Color) -> button::Style {
    button::Style {
        background: Some(Background::Color(background)),
        text_color,
        border: border(RADIUS_SM, 1.0, border_color),
        ..button::Style::default()
    }
}

fn primary_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => accent_hover(),
        button::Status::Pressed => accent_pressed(),
        button::Status::Disabled => Color::from_rgb8(206, 213, 224),
        button::Status::Active => accent(),
    };
    base_button_style(background, Color::WHITE, background)
}

fn secondary_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => panel_background_hover(),
        button::Status::Pressed => accent_soft(),
        button::Status::Disabled => surface_alt(),
        button::Status::Active => surface(),
    };
    base_button_style(background, primary_text(), border_color())
}

fn method_button_style(selected: bool, status: button::Status) -> button::Style {
    let background = match (selected, status) {
        (true, button::Status::Pressed) => accent_pressed(),
        (true, button::Status::Hovered) => accent_hover(),
        (true, _) => accent(),
        (false, button::Status::Hovered) => panel_background_hover(),
        (false, button::Status::Pressed) => accent_soft(),
        _ => surface(),
    };
    let border_color = if selected { accent() } else { border_color() };
    base_button_style(
        background,
        if selected { Color::WHITE } else { primary_text() },
        border_color,
    )
}

fn menu_button_style(active: bool, status: button::Status) -> button::Style {
    let background = match (active, status) {
        (true, _) => accent_soft(),
        (false, button::Status::Hovered) => panel_background_hover(),
        (false, button::Status::Pressed) => accent_soft(),
        _ => transparent(),
    };
    base_button_style(background, primary_text(), transparent())
}

/// The whole-tab pill: an accent-bordered surface when active, a flat chip
/// otherwise. The title and close controls share this single background.
/// Tab pill style; `drop_target` highlights the tab a dragged tab will drop onto.
fn tab_container_style_dnd(active: bool, dragging: bool) -> container::Style {
    let background = if dragging {
        accent_soft()
    } else if active {
        surface()
    } else {
        surface_alt()
    };
    let border_color = if active || dragging {
        accent()
    } else {
        border_color()
    };
    container::Style {
        background: Some(Background::Color(background)),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, if dragging { 2.0 } else { 1.0 }, border_color),
        // Lift the dragged tab so it reads as picked up while it slides.
        shadow: if dragging {
            subtle_shadow()
        } else {
            Shadow::default()
        },
        ..container::Style::default()
    }
}

/// Subtle close glyph that hugs the title and lifts gently on hover.
fn tab_close_button_style(status: button::Status) -> button::Style {
    let (background, text_color) = match status {
        button::Status::Hovered | button::Status::Pressed => {
            (panel_background_hover(), primary_text())
        }
        _ => (transparent(), muted_text()),
    };
    base_button_style(background, text_color, transparent())
}

fn close_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => danger_background(),
        button::Status::Pressed => Color::from_rgb8(250, 220, 220),
        _ => transparent(),
    };
    let text_color = match status {
        button::Status::Hovered | button::Status::Pressed => danger(),
        _ => muted_text(),
    };
    base_button_style(background, text_color, transparent())
}

fn menu_command_button_style(status: button::Status) -> button::Style {
    secondary_button_style(status)
}

fn toolbar_icon_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => panel_background_hover(),
        button::Status::Pressed => accent_soft(),
        _ => transparent(),
    };
    base_button_style(background, primary_text(), transparent())
}

fn toolbar_separator_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(border_color())),
        ..container::Style::default()
    }
}

fn toolbar_input_style(_theme: &Theme, status: text_input::Status) -> text_input::Style {
    let border_color = match status {
        text_input::Status::Focused { .. } => accent(),
        text_input::Status::Hovered => border_strong(),
        text_input::Status::Active | text_input::Status::Disabled => border_color(),
    };

    text_input::Style {
        background: Background::Color(field_background()),
        border: border(RADIUS_SM, 1.0, border_color),
        icon: muted_text(),
        placeholder: muted_text(),
        value: primary_text(),
        selection: accent_soft(),
    }
}

fn sidebar_header_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface_alt())),
        text_color: Some(muted_text()),
        border: border(0.0, 1.0, border_color()),
        ..container::Style::default()
    }
}

fn sidebar_tool_button_style(status: button::Status) -> button::Style {
    toolbar_icon_button_style(status)
}

fn group_row_style(drop_target: bool) -> container::Style {
    let background = if drop_target {
        accent_soft()
    } else {
        transparent()
    };
    let border_color = if drop_target { accent() } else { transparent() };

    container::Style {
        background: Some(Background::Color(background)),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.0, border_color),
        ..container::Style::default()
    }
}

fn tree_item_container_style(selected: bool, hovered: bool, dragging: bool) -> container::Style {
    // The dragged row gets a clear "lifted" accent (soft fill + thicker accent
    // border) so, together with its live slide under the cursor, the drag is
    // obvious.
    let background = if dragging || selected {
        accent_soft()
    } else if hovered {
        panel_background_hover()
    } else {
        transparent()
    };

    let border_color = if dragging || selected {
        accent()
    } else {
        transparent()
    };

    container::Style {
        background: Some(Background::Color(background)),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, if dragging { 2.0 } else { 1.0 }, border_color),
        // A shadow "lifts" the dragged row off the list so it reads as picked up.
        shadow: if dragging {
            soft_shadow()
        } else {
            Shadow::default()
        },
        ..container::Style::default()
    }
}

fn profile_context_menu_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.0, border_color()),
        shadow: subtle_shadow(),
        ..container::Style::default()
    }
}

fn profile_context_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => panel_background_hover(),
        button::Status::Pressed => accent_soft(),
        button::Status::Disabled => surface_alt(),
        button::Status::Active => surface(),
    };

    base_button_style(background, primary_text(), transparent())
}

/// A vertical context-menu row: left-aligned, subtle hover, red for destructive.
fn profile_menu_item_style(status: button::Status, destructive: bool) -> button::Style {
    let hover_bg = if destructive {
        danger_background()
    } else {
        panel_background_hover()
    };
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => hover_bg,
        _ => transparent(),
    };
    let text_color = if destructive { danger() } else { primary_text() };
    let mut style = base_button_style(background, text_color, transparent());
    style.border = border(RADIUS_SM - 2.0, 0.0, transparent());
    style
}

fn profile_edit_menu_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(RADIUS_MD, 1.0, border_color()),
        ..container::Style::default()
    }
}

fn status_color(status: SessionStatus) -> Color {
    match status {
        SessionStatus::Connecting => Color::from_rgb8(245, 158, 11),
        SessionStatus::Connected => Color::from_rgb8(34, 197, 94),
        SessionStatus::Disconnected => muted_text(),
        SessionStatus::Error => danger(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::key::{Code, Physical};

    #[test]
    fn avatar_initials_takes_up_to_two_tokens() {
        assert_eq!(avatar_initials("prod-web-01"), "PW");
        assert_eq!(avatar_initials("local lab"), "LL");
        assert_eq!(avatar_initials("redis"), "R");
        assert_eq!(avatar_initials(""), "?");
    }

    #[test]
    fn render_log_name_substitutes_name_and_host() {
        // Host is parsed out of the user@host:port endpoint.
        assert_eq!(
            render_log_name("%N@%H.log", "web01", "root@10.0.0.5:22"),
            "web01@10.0.0.5.log"
        );
        // An endpoint without a user part still yields the host.
        assert_eq!(render_log_name("%H", "x", "COM3"), "COM3");
        // Date/time tokens are all replaced (no literal % left) and expand to
        // the expected width.
        let dated = render_log_name("%Y-%M-%D", "x", "h");
        assert!(!dated.contains('%'));
        assert_eq!(dated.len(), "2026-07-08".len());
    }

    #[test]
    fn mouse_events_encode_sgr_and_x10() {
        // SGR (1006): ESC[<cb;col;row(M|m), 1-based coords.
        assert_eq!(encode_mouse_event(true, 0, 0, 0, true, false), b"\x1b[<0;1;1M");
        assert_eq!(encode_mouse_event(true, 0, 4, 2, false, false), b"\x1b[<0;5;3m");
        // Drag adds 32 to the button code.
        assert_eq!(encode_mouse_event(true, 0, 9, 1, true, true), b"\x1b[<32;10;2M");
        // Wheel up / down.
        assert_eq!(encode_mouse_event(true, 64, 0, 0, true, false), b"\x1b[<64;1;1M");
        // Legacy X10: ESC [ M (cb+32) (col+1+32) (row+1+32).
        assert_eq!(
            encode_mouse_event(false, 0, 0, 0, true, false),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
    }

    #[test]
    fn version_compare_detects_newer_releases() {
        assert!(version_is_newer("v0.1.10", "0.1.9"));
        assert!(version_is_newer("0.2.0", "0.1.9"));
        assert!(version_is_newer("v1.0.0", "0.9.9"));
        assert!(!version_is_newer("v0.1.9", "0.1.9"));
        assert!(!version_is_newer("v0.1.8", "0.1.9"));
        // Malformed parts degrade to 0 rather than panicking.
        assert!(!version_is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn pane_grid_dims_tiles_by_count() {
        use TileMode::*;
        assert_eq!(pane_grid_dims(1, Grid), (1, 1));
        assert_eq!(pane_grid_dims(2, Grid), (2, 1));
        assert_eq!(pane_grid_dims(3, Grid), (3, 1));
        assert_eq!(pane_grid_dims(4, Grid), (2, 2));
        assert_eq!(pane_grid_dims(6, Grid), (3, 2));
        // Columns = all side by side; Rows = all stacked.
        assert_eq!(pane_grid_dims(4, Columns), (4, 1));
        assert_eq!(pane_grid_dims(4, Rows), (1, 4));
    }

    #[test]
    fn command_input_delta_tracks_typing_and_erasing() {
        // Appended text -> send the suffix.
        assert_eq!(command_input_delta("ls", "ls -"), Some(b" -".to_vec()));
        assert_eq!(command_input_delta("", "a"), Some(b"a".to_vec()));
        // Erased text -> one DEL per removed char.
        assert_eq!(command_input_delta("ls -l", "ls"), Some(vec![0x7f, 0x7f, 0x7f]));
        // No change -> nothing to send.
        assert_eq!(command_input_delta("ls", "ls"), Some(Vec::new()));
        // A mid-string edit can't be a simple keystroke -> None (don't send).
        assert_eq!(command_input_delta("cat a.txt", "cat b.txt"), None);
    }

    #[test]
    fn word_bounds_selects_whole_tokens() {
        // Double-click inside a word grabs the whole word.
        assert_eq!(word_bounds("hello world", 1), Some((0, 5)));
        assert_eq!(word_bounds("hello world", 8), Some((6, 11)));
        // Path-like tokens stay a single word (/, ., -, ~ are word chars).
        assert_eq!(word_bounds("cd /usr/local/bin", 8), Some((3, 17)));
        assert_eq!(word_bounds("see ./a.tar.gz now", 6), Some((4, 14)));
        // On a space/separator, only that one cell is selected.
        assert_eq!(word_bounds("a b", 1), Some((1, 2)));
        // Clicking past the end of the line selects nothing.
        assert_eq!(word_bounds("hi", 5), None);
    }

    #[test]
    fn terminal_size_for_area_clamps_to_sane_bounds() {
        // A tiny area still yields the minimum grid, not zero.
        let tiny = terminal_size_for_area(1.0, 1.0);
        assert_eq!(tiny.cols, 20);
        assert_eq!(tiny.rows, 6);
        // A generous area scales up but stays under the ceiling.
        let big = terminal_size_for_area(100_000.0, 100_000.0);
        assert_eq!(big.cols, 220);
        assert_eq!(big.rows, 80);
    }

    #[test]
    fn pane_body_origin_places_each_cell_of_the_grid() {
        // A 2x2 layout: verify column/row offsets and the header shift.
        let layout = PaneLayout {
            cols: 2,
            pane_w: 400.0,
            pane_h: 300.0,
            origin_x: 348.0,
            origin_y: 98.0,
            header: 26.0,
        };
        // Top-left pane body starts at origin + header.
        assert_eq!(layout.pane_body_origin(0), Point::new(348.0, 124.0));
        // Top-right shifts one column (pane_w + gap).
        assert_eq!(
            layout.pane_body_origin(1),
            Point::new(348.0 + 400.0 + PANE_GAP, 124.0)
        );
        // Bottom-left shifts one row (pane_h + gap).
        assert_eq!(
            layout.pane_body_origin(2),
            Point::new(348.0, 98.0 + 300.0 + PANE_GAP + 26.0)
        );
    }

    #[test]
    fn sftp_cmp_orders_by_column_and_direction() {
        use std::cmp::Ordering;
        let a = ("alpha", 10u64, Some(100u64));
        let b = ("beta", 5u64, Some(200u64));
        // Name ascending: alpha < beta.
        assert_eq!(sftp_cmp(SftpSortKey::Name, true, a, b), Ordering::Less);
        // Name descending flips it.
        assert_eq!(sftp_cmp(SftpSortKey::Name, false, a, b), Ordering::Greater);
        // Size ascending: 10 > 5.
        assert_eq!(sftp_cmp(SftpSortKey::Size, true, a, b), Ordering::Greater);
        // Modified ascending: 100 < 200.
        assert_eq!(sftp_cmp(SftpSortKey::Modified, true, a, b), Ordering::Less);
    }

    #[test]
    fn format_epoch_utc_matches_known_timestamps() {
        assert_eq!(format_epoch_utc(0), "1970-01-01 00:00");
        assert_eq!(format_epoch_utc(1_609_459_200), "2021-01-01 00:00"); // 2021-01-01 UTC
        assert_eq!(format_epoch_utc(1_703_980_800), "2023-12-31 00:00"); // 2023-12-31 UTC
        assert_eq!(sftp_date(None), "—");
    }

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
                y: -cell_height()
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
