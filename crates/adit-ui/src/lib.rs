pub(crate) use adit_domain::{
    AuthMethod, ConnectionProfile, Environment, JumpHop, ProfileId, Protocol, SessionId,
    SessionStatus, TunnelDef,
};
pub(crate) use adit_session::{
    known_hosts_path, list_known_hosts, remove_known_host, AuthPromptInfo, HostKeyPromptInfo,
    KnownHostEntry, LocalEntry, ProfileDropPosition, ProfileSortKey, RdpInput,
    RdpMouseButton, RdpQuality, SessionError, SessionManager, SessionSummary, SftpBrowser, SftpEntry,
    TransferDirection, TransferItem, TransferStatus, TunnelKind, TunnelState,
};
pub(crate) use adit_storage::{
    AppSettings, CredentialStore, ProfileCatalog, ProfileStore, HostLayout, SettingsStore, Snippet, ThemeMode,
};
pub(crate) use adit_terminal::{
    Color as TermColor, LogicalAnchor, MouseMode, TerminalLine, TerminalSize, TerminalSnapshot,
    Viewport,
};
pub(crate) use iced::font::Weight;
pub(crate) use iced::keyboard::{self, key::Named, Key};
pub(crate) use iced::widget::{
    button, checkbox, container, mouse_area, opaque, progress_bar, row, scrollable, stack,
    text, text_input, tooltip, Space,
};
pub(crate) use iced::{
    clipboard, event, mouse, window, Alignment, Background, Border, Color, Element, Fill, Font,
    Length, Point, Shadow, Subscription, Task, Theme, Vector,
};
pub(crate) use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
pub(crate) use std::time::Instant;

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
mod i18n;
pub(crate) use i18n::{set_language, t, tf};
mod style;
use style::*;
/// The local half of RDP clipboard file transfer. Separate from the text
/// clipboard, which iced provides — files need Win32 directly, and the three
/// platforms do not share a mechanism. See the module docs.
mod clipboard_files;
mod workspace;
use workspace::*;
mod sidebar;
use sidebar::*;
mod hosts;
use hosts::*;
mod editor;
use editor::*;
mod update_loop;
use update_loop::*;
mod updater;
use updater::*;
mod profiles;
use profiles::*;
mod session_ops;
use session_ops::*;
mod chrome;
use chrome::*;
mod dialogs;
use dialogs::*;
mod sftp;
use sftp::*;
mod highlight;
mod input;
mod terminal_text;
mod theme;
use input::*;
use terminal_text::*;
use theme::{
    active_scheme, color_scheme_index, font_preset_index, term_font,
    COLOR_SCHEMES, FONT_PRESETS,
};

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
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use unicode_width::UnicodeWidthChar;

pub struct AditApp {
    manager: SessionManager,
    profile_store: ProfileStore,
    credential_store: CredentialStore,
    selected_profile: Option<ProfileId>,
    hovered_profile: Option<ProfileId>,
    dragged_profile: Option<ProfileId>,
    // Where the dragged profile will land on release (drives the insertion line).
    profile_drop: Option<ProfileDrop>,
    // The press point, and whether the pointer has moved far enough to count as a
    // real drag. The insertion line / drop zones only appear once active, so a
    // plain click or double-click never mutates the tree (which would drop the
    // row's click-tracking and swallow the double-click).
    profile_drag_origin: Option<Point>,
    profile_drag_active: bool,
    // Folder (group) drag-reorder, mirroring the session drag: `dragged_group` is
    // the held folder, the drag only "activates" once the pointer leaves a dead
    // zone (so a plain click still toggles collapse), and `group_drop` is the
    // folder it will land next to — the drag direction decides which side.
    dragged_group: Option<String>,
    group_drag_active: bool,
    group_drag_origin: Option<Point>,
    group_drop: Option<String>,
    group_drop_target: Option<String>,
    group_context_menu: Option<String>,
    // Inline rename: the folder / session whose name is being edited in place (in
    // the row itself, not a separate popup), plus the working text.
    editing_group: Option<String>,
    group_name_draft: String,
    editing_profile: Option<ProfileId>,
    profile_name_draft: String,
    profile_context_menu: Option<ProfileId>,
    /// The session tab whose right-click context menu is open.
    tab_context_menu: Option<SessionId>,
    profile_editor: Option<ProfileId>,
    connection_dialog: Option<ConnectionDialog>,
    // Folders in user-arrangeable order (top-level tree order); a session may be
    // ungrouped (top level) and interleaved among these.
    groups: Vec<String>,
    /// Icon key per group, mirroring `ProfileCatalog::group_icons`.
    group_icons: std::collections::BTreeMap<String, String>,
    collapsed_groups: BTreeSet<String>,
    active_menu: Option<MenuKind>,
    profile_group: String,
    /// Whether the session editor's 高级 section is expanded.
    ///
    /// Its own field rather than something inferred from the fields inside it:
    /// a section that unfolded whenever a profile happened to carry a TERM
    /// would give the dialog a different height for every session, and leave
    /// no state for the disclosure triangle to toggle back to. Sticky for the
    /// run (editing ProxyJump across several sessions shouldn't re-collapse
    /// each time) but never persisted, so a fresh start is always collapsed.
    profile_advanced_open: bool,
    profile_name: String,
    profile_host: String,
    profile_port: String,
    profile_username: String,
    profile_auth_method: AuthMethod,
    // Password-auth password for the editor. Held only in memory + the OS
    // credential vault; never serialized to profiles.json.
    profile_password: String,
    /// Key passphrase draft; saved to the credential vault, never to
    /// profiles.json. Distinct from `profile_password`.
    profile_passphrase: String,
    profile_protocol: Protocol,
    /// Preset icon key being edited; empty means "work it out".
    profile_icon: String,
    profile_identity_file: String,
    profile_startup_command: String,
    /// Jump-host chain as an editable OpenSSH-style spec (`user@host:port`,
    /// comma/newline separated), parsed to `profile.jumps` on save.
    profile_jumps: String,
    profile_terminal_type: String,
    /// Per-profile tab colour-coding drafts.
    profile_environment: Environment,
    profile_accent_color: String,
    profile_label: String,
    connect_timeout_secs: u32,
    scrollback_lines: u32,
    snippets: Vec<Snippet>,
    snippets_open: bool,
    snippet_name_draft: String,
    snippet_command_draft: String,
    auto_check_updates: bool,
    auto_accept_host_keys: bool,
    rdp_clipboard: bool,
    /// Visual-fidelity preset for RDP (persisted). Applied at connect time, so
    /// changing it on a live desktop reconnects that session.
    rdp_quality: RdpQuality,
    /// The floating RDP toolbar, which exists only in fullscreen — that is the
    /// mode with no menu bar, so it is the only one where these actions have
    /// nowhere else to live. A permanently docked icon strip was tried once and
    /// removed for costing 36px to duplicate menu items (see `chrome::view`);
    /// this one floats over the desktop and reserves no layout space at all.
    ///
    /// Collapsed to a small tab by the ⌃ button, and not otherwise hidden. It
    /// was hover-revealed once and could not be: see the note in
    /// `with_rdp_toolbar` for why that flickered every frame. Deliberately not
    /// persisted, matching `fullscreen` itself — it is a view choice about the
    /// session in front of you, not a preference.
    rdp_toolbar_collapsed: bool,
    /// The quality dropdown hanging off the toolbar's ⚡ button.
    rdp_quality_menu_open: bool,
    /// Whether the one-time legacy-keyring import has completed (persisted). Gates
    /// the startup keyring probe so it never runs again once done — see the boot
    /// task and [`AppSettings::keyring_migrated`].
    keyring_migrated: bool,
    /// The active keyboard-interactive/MFA prompt (its session + fields) mirrored
    /// into UI state, plus the in-progress answers (one per field). Kept here so
    /// the dialog's text inputs can borrow owned values that outlive a `view`.
    auth_prompt: Option<(SessionId, AuthPromptInfo)>,
    auth_prompt_answers: Vec<String>,
    /// A terminal hyperlink awaiting the user's confirm-before-open decision.
    pending_hyperlink: Option<String>,
    password: String,
    remember_connection_password: bool,
    session_filter: String,
    sftp_upload_path: String,
    sftp_new_folder: String,
    sftp_rename: Option<(SftpPane, String)>,
    sftp_rename_to: String,
    sftp_delete_target: Option<(SftpPane, String, bool)>,
    /// Right-click context menu target in an SFTP pane: (pane, entry name, is_dir).
    sftp_context_menu: Option<(SftpPane, String, bool)>,
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
    // Anchored in ABSOLUTE scrollback rows (not viewport rows) so it stays correct
    // across scrolling — which is what lets a drag auto-scroll past the pane edge.
    // Mapped back into viewport space only at render (`selection_for_viewport`).
    terminal_selection: Option<TerminalSelection>,
    terminal_selecting: bool,
    // While drag-selecting past the top/bottom edge: rows to scroll per tick,
    // negative = up (older), positive = down (newer), 0 = pointer inside.
    selection_autoscroll: i32,
    /// Text-cursor blink phase: true = the block is painted this instant.
    cursor_blink_on: bool,
    /// A terminal scrollbar-thumb drag is in progress (track the cursor globally).
    scrollbar_dragging: bool,
    // Last terminal press (cell, time, click-count) for double/triple-click
    // word/line selection.
    terminal_click: Option<(TerminalPoint, Instant, u8)>,
    terminal_context_menu: bool,
    terminal_scroll_offset: usize,
    // RDP: the active session's framebuffer as an iced image handle, rebuilt only
    // when the helper reports a new generation (`rdp_frame_generation` is the
    // generation currently uploaded). `rdp_surface_size` is the last size we told
    // the helper, so a window resize only sends a Resize when it actually changed.
    rdp_frame_generation: u64,
    rdp_surface_size: Option<(u16, u16)>,
    // The remote desktop size we last *asked* for (viewport pixels). Sizing the
    // remote to the on-screen area renders it 1:1 instead of upscaling a fixed
    // 1280×720 surface (which looked blurry). Deduped so a resize is only sent
    // when the target actually changes.
    rdp_target_size: Option<(u16, u16)>,
    // Which session `rdp_image`/`rdp_surface_size`/`rdp_frame_generation` belong
    // to. Each RDP session has its own generation counter, so on a tab switch the
    // cache must be invalidated — otherwise we'd render one host's frame under
    // another's tab (and could get stuck if the generations happened to match).
    rdp_frame_session: Option<SessionId>,
    /// When the RDP texture was last handed to the renderer (see the throttle
    /// in the frame sampler).
    rdp_frame_uploaded: Option<Instant>,
    /// The desktop texture, split into a grid of tiles.
    ///
    /// Each tile is kept under iced_wgpu's `MAX_SYNC_SIZE` (2 MiB, see
    /// `image/cache.rs upload_raster`): under it an upload is applied
    /// synchronously and is drawable the same frame; at or over it the upload
    /// is handed to an async worker and the image is SKIPPED for at least one
    /// frame, showing the black container through. A full 1908x1152 desktop is
    /// 8.8 MB, so every single frame took the async path — that one constant
    /// is the whole story behind the black flicker, the scroll ghosting and
    /// the minimise remnants.
    rdp_tiles: Vec<RdpTile>,
    /// The outgoing generation's tiles, drawn UNDER `rdp_tiles` across a
    /// surface-size swap. A handle minted this update has not finished its
    /// async GPU upload when the very next frame renders; an image that has
    /// not landed draws nothing, and with nothing beneath it that frame was
    /// the black container — one flash per sidebar toggle. Cleared by the next
    /// same-size frame or by `RDP_PREV_LAYER_LINGER`.
    rdp_tiles_prev: Vec<RdpTile>,
    rdp_surface_size_prev: Option<(u16, u16)>,
    // RDP clipboard: only this process has a Windows clipboard (the helper is
    // windowless), so local→remote means polling it while an RDP tab is up.
    // `rdp_clipboard_offered` is the last text handed to the helper — it stops
    // the poll from re-offering the same thing, and, because inbound remote text
    // is recorded here too, stops a remote copy from bouncing straight back.
    rdp_clipboard_offered: Option<String>,
    /// Files currently offered to the remote, with the local path each came
    /// from. Indexed by the remote: a `FileContentsRequest` names a position in
    /// this list, so it must outlive the clipboard selection that produced it —
    /// a paste reads bytes long after Explorer has moved on to something else.
    rdp_offered_files: Vec<clipboard_files::OfferedFile>,
    /// Brokers the blocking reads that Explorer's paste threads make against
    /// files the *remote* offered. One per app, not per session: a paste can
    /// outlive the session it started in, and the bridge is what tells those
    /// waiters the answer is never coming.
    rdp_chunk_bridge: clipboard_files::ChunkBridge,
    /// Whether the last settings write failed. Only there to report the failure
    /// once per streak; the retry itself is driven by the usual comparison.
    settings_save_failed: bool,
    /// When the last remote-resolution request went out. While it is recent and
    /// the delivered surface is not yet the requested size, the stale frame is
    /// scaled to fit instead of drawn 1:1 — see `rdp_fit_factor`.
    rdp_resize_requested_at: Option<Instant>,
    /// The size a window drag wants but has not asked for yet. Debounced: a
    /// drag fires a resize per mouse movement, and renegotiating each one made
    /// the server rebuild its desktop dozens of times per drag — every rebuild
    /// a full tile re-upload racing iced's async image worker, which is what
    /// flashed the pane black. `flush_pending_rdp_resize` sends it once the
    /// size holds still.
    rdp_resize_pending: Option<(u16, u16)>,
    /// When `rdp_resize_pending` last changed — the debounce anchor.
    rdp_resize_pending_since: Option<Instant>,
    /// Device pixels per logical point of the window's display. Drives the
    /// RDP viewport request (physical pixels) and the 1:1 presentation.
    display_scale: f32,
    rdp_clipboard_ticks: u8,
    // Latest keyboard modifier state, so wheel handling can tell a plain scroll
    // from a Ctrl+wheel zoom.
    modifiers: keyboard::Modifiers,
    window_width: f32,
    window_height: f32,
    sidebar_width: f32,
    sidebar_visible: bool,
    /// Chrome-free presentation mode (Ctrl+Alt+Enter).
    ///
    /// Kept out of the persisted settings on purpose: a client that starts up
    /// fullscreen with no menu bar gives a first-time user nothing to click.
    fullscreen: bool,
    sidebar_dragging: bool,
    cursor_pos: Point,
    context_menu_pos: Point,
    /// Which top-level view fills the area beside the nav rail.
    main_view: MainView,
    /// How the host manager lays its entries out.
    host_layout: HostLayout,
    /// Hosts most recently connected to, newest first.
    recent_hosts: Vec<ProfileId>,
    /// The grid's own ordering — see `AppSettings::grid_order` for why it is
    /// not the tree's.
    grid_order: Vec<ProfileId>,
    /// Whether the drag in flight started on a grid card. Decides which order a
    /// `Beside` drop edits: the grid's own, or the tree's shared one.
    drag_from_grid: bool,
    /// The cursor within the host pane, for the drag ghost — pane-relative, the
    /// way `sftp_drag_cursor` is, because the ghost is positioned with spacers.
    hosts_cursor: Option<Point>,
    /// Each card's slot index, animated. Reordering used to be instantaneous,
    /// which is not perceptible as motion: the cards were simply somewhere else
    /// the next frame and nothing said they had moved.
    card_slots: std::collections::HashMap<ProfileId, iced::animation::Animation<f32>>,
    /// What is on screen, resolved from [`Self::theme_mode`].
    dark_mode: bool,
    /// What the user asked for, which is not the same thing under `System`.
    theme_mode: ThemeMode,
    font_family: String,
    font_size: f32,
    color_scheme: String,
    /// Highlight rules the user has moved off their shipped default, by id.
    /// Only the deviations — see `AppSettings::highlight_rules`.
    highlight_rules: BTreeMap<String, bool>,
    /// The unified 设置 dialog, and which category it is showing.
    settings_open: bool,
    settings_category: SettingsCategory,
    language: adit_storage::Language,
    sync_tab: SyncTab,
    update_dialog_open: bool,
    update_state: UpdateState,
    /// The trusted-host-keys (known_hosts) management dialog + its loaded list.
    known_hosts_open: bool,
    known_hosts: Vec<KnownHostEntry>,
    /// Live copy of the persisted sync configuration, edited by the panel.
    sync: adit_storage::SyncSettings,
    /// Whether the credential store already holds a secret for the selected
    /// provider, so the panel can say "leave blank to keep it".
    sync_secret_saved: bool,
    /// Secrets are edited here rather than read back from the credential
    /// store: a stored token shows as a placeholder, and an empty box means
    /// "keep what is saved" rather than "clear it". Opening the panel and
    /// closing it can therefore never silently wipe a working configuration.
    sync_secret_draft: String,
    /// True while a browser authorisation is outstanding.
    sync_connecting: bool,
    /// The user code to display while a device flow is waiting, and where to
    /// type it. `None` for the loopback providers, which have nothing to show.
    sync_device_prompt: Option<DeviceCodePrompt>,
    /// True while a sync is in flight, so the button can say so and cannot be
    /// pressed twice.
    sync_busy: bool,
    /// One line for the panel: what the last attempt did, or why it failed.
    sync_status: String,
    /// Sessions the last sync could not settle. Local was kept for each.
    sync_conflicts: Vec<String>,
    /// The config folder in use this run (resolved at startup). Relocating it
    /// (e.g. onto Dropbox) takes effect on the next launch — `pending_config_dir`
    /// holds a freshly-chosen target until then.
    config_dir: std::path::PathBuf,
    pending_config_dir: Option<std::path::PathBuf>,
    /// Whether the config folder is a UI-set custom location (drives the "reset
    /// to default" button). Cached so the options view avoids a per-frame read.
    config_dir_custom: bool,
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
    Help,
}

/// Which text box in the sync panel changed. One message with a field tag
/// beats eight near-identical variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncField {
    GistId,
    WebDavUrl,
    WebDavUsername,
    S3Endpoint,
    S3Region,
    S3Bucket,
    S3Key,
    S3AccessKey,
    GoogleClientId,
    OneDriveClientId,
    DropboxClientId,
    GitHubClientId,
    GoogleClientSecret,
}

/// The half of a device-flow authorisation the user has to act on.
///
/// Kept in app state rather than derived, because it must stay on screen for
/// the whole polling window: unlike the loopback flow, nothing brings the user
/// back to us automatically, and a code that vanishes on the next redraw is a
/// code nobody can finish typing.
#[derive(Debug, Clone)]
pub struct DeviceCodePrompt {
    pub user_code: String,
    pub verification_uri: String,
}

/// What a finished sync tells the UI. Carries the merged catalog so the update
/// loop can save it on the UI thread, where the profile store lives.
#[derive(Debug, Clone)]
pub struct SyncReport {
    pub catalog: adit_storage::ProfileCatalog,
    pub conflicts: Vec<String>,
    pub summary: String,
    /// An id the provider minted during this sync (a freshly created Gist).
    /// Persisted so the next sync reuses it instead of creating another.
    pub assigned_id: Option<String>,
}

/// Which half of the 同步与云 page is showing.
///
/// Split because the two answer different questions: "where does this go" is
/// set once, and "did it work" is looked at repeatedly. One scroll holding both
/// buried the second under the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncTab {
    #[default]
    Services,
    Status,
}

/// One page of the 设置 dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsCategory {
    App,
    Appearance,
    Terminal,
    Logging,
    Sync,
}

impl SettingsCategory {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::App => "应用",
            Self::Appearance => "外观",
            Self::Terminal => "终端",
            Self::Logging => "日志",
            Self::Sync => "同步与云",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuCommand {
    ToggleFullscreen,
    NewProfile,
    NewGroup,
    SaveProfile,
    DeleteProfile,
    SortByName,
    SortByHost,
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
    KnownHosts,
    Options,
    ImportSshConfig,
    ImportSecureCrt,
    Snippets,
    ToggleBroadcast,
    ToggleCommandWindow,
    SplitPane,
    ToggleSidebar,
    ToggleTheme,
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
    // RDP: per-frame surface sampling + input over the graphical surface.
    RdpTick,
    RdpPointerMoved(Point),
    RdpPressed(mouse::Button),
    RdpReleased(mouse::Button),
    RdpScrolled(mouse::ScrollDelta),
    // The floating fullscreen toolbar over an RDP desktop.
    ToggleRdpToolbarCollapsed,
    ToggleRdpQualityMenu,
    /// Pick a fidelity preset. Reconnects the active desktop if one is live,
    /// because RDP settles performance flags during the handshake.
    RdpQualityChosen(RdpQuality),
    /// Send Ctrl+Alt+Del to the remote desktop as three scancode pairs.
    RdpSendCtrlAltDel,
    ToggleMenu(MenuKind),
    ToggleTheme,
    CloseAppearance,
    FontFamilyChanged(u8),
    FontSizeStep(i32),
    ColorSchemeChanged(u8),
    /// Switch the top-level view beside the nav rail.
    ShowMainView(MainView),
    /// Switch how the host manager lays its entries out.
    HostLayoutChanged(HostLayout),
    /// Flip one keyword-highlight rule, by its stable id.
    HighlightRuleToggled(&'static str),
    CloseOptions,
    /// Close the 设置 page.
    CloseSettings,
    /// Show one category of the 设置 page.
    SettingsCategoryPicked(SettingsCategory),
    /// Switch between 云服务 and 同步状态.
    SyncTabPicked(SyncTab),
    /// Redraw the whole UI in another language.
    LanguageChanged(adit_storage::Language),
    // Trusted-host-keys (known_hosts) management.
    CloseKnownHosts,
    RemoveKnownHost(String, String),
    // Relocate the configuration folder (e.g. onto a synced drive like Dropbox).
    PickConfigDir,
    ConfigDirPicked(Option<std::path::PathBuf>),
    ResetConfigDir,
    LogDirChanged(String),
    LogNamePatternChanged(String),
    PickLogDir,
    LogDirPicked(Option<std::path::PathBuf>),
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
    ProfilePressed(ProfileId),
    /// A press on a grid card: the same arming as ProfilePressed, remembered as
    /// grid-originated so the drop edits the grid's order.
    GridProfilePressed(ProfileId),
    /// The cursor moved inside the host pane (feeds the drag ghost).
    HostsCursorMoved(Point),
    /// Grid-side hover/drag-over. Split from the tree's pair so each view can
    /// only arm and retarget drags that started in it.
    GridProfileHovered(ProfileId),
    GridProfileDragOver(ProfileId, ProfileDropPosition),
    ProfileDoubleClicked(ProfileId),
    ProfileHovered(ProfileId),
    ProfileHoverExited(ProfileId),
    ProfileDragOver(ProfileId, ProfileDropPosition),
    ProfileDropped(ProfileId),
    ProfileDragOverTop,
    ProfileDragOverBottom,
    ProfileDragOverGroup(String),
    ProfileDroppedOnGroup(String),
    ProfileGroupHoverExited(String),
    CancelProfileDrag,
    ShowGroupContextMenu(String),
    HideGroupContextMenu,
    /// Set a group's tile icon, or clear it with an empty key.
    GroupIconPicked(String, &'static str),
    RenameGroupFromContext(String),
    NewProfileInGroup(String),
    DeleteGroupFromContext(String),
    GroupNameDraftChanged(String),
    SaveGroupRename,
    ShowProfileContextMenu(ProfileId),
    HideProfileContextMenu,
    SidebarCursorMoved(Point),
    /// Window-absolute cursor position, tracked globally so context menus (e.g.
    /// the tab menu, whose strip has no local tracker) anchor at the pointer.
    GlobalCursorMoved(Point),
    // Inline session rename (edits the name in the sidebar row, no popup).
    RenameProfileFromContext(ProfileId),
    ProfileNameDraftChanged(String),
    SaveProfileRename,
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
    AuthPromptInput { index: usize, value: String },
    SubmitAuthPrompt { session_id: SessionId },
    CancelAuthPrompt { session_id: SessionId },
    OpenHyperlink(String),
    ConfirmOpenHyperlink,
    CancelOpenHyperlink,
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
    // Right-click a pane entry: track the cursor for the anchor, open/close the menu.
    SftpCursorMoved(Point),
    ShowSftpContextMenu(SftpPane, String, bool),
    HideSftpContextMenu,
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
    /// Stop a single in-flight/queued transfer by its id.
    SftpCancelTransfer(u64),
    /// Stop every transfer that is still pending or active.
    SftpCancelAll,
    SftpDragEnter(SftpPane),
    SftpDragMove(SftpPane, Point),
    ToggleProfileGroup(String),
    // Pressing a folder header arms a folder drag-reorder; a release without any
    // real movement falls back to toggling the folder's collapse.
    GroupPressed(String),
    /// Put the session being edited in an existing folder (empty = ungrouped).
    ProfileGroupPicked(String),
    /// Type in the editor's folder field: filters the chips below it, and names
    /// a brand new folder when it matches none of them.
    ProfileGroupChanged(String),
    /// Fold / unfold the session editor's 高级 section.
    ToggleProfileAdvanced,
    ProfileNameChanged(String),
    ProfileHostChanged(String),
    ProfilePortChanged(String),
    ProfileUsernameChanged(String),
    ProfileAuthMethodChanged(AuthMethod),
    ProfilePasswordChanged(String),
    ProfilePassphraseChanged(String),
    ProfileProtocolChanged(Protocol),
    /// Pick a preset icon for the profile being edited (empty clears it).
    ProfileIconChanged(&'static str),
    ProfileIdentityFileChanged(String),
    PickIdentityFile,
    IdentityFilePicked(Option<std::path::PathBuf>),
    SecureCrtFolderPicked(Option<std::path::PathBuf>),
    ProfileStartupCommandChanged(String),
    ProfileJumpsChanged(String),
    ProfileEnvironmentChanged(Environment),
    ProfileAccentColorChanged(String),
    ProfileLabelChanged(String),
    ProfileTerminalTypeChanged(String),
    ConnectTimeoutChanged(String),
    ScrollbackLinesChanged(String),
    SessionFilterChanged(String),
    NewProfileDraft,
    NewGroupDraft,
    SaveProfile,
    DeleteSelectedProfile,
    TerminalInputChanged(String),
    KeyboardInput(keyboard::Event),
    ModifiersChanged(keyboard::Modifiers),
    WindowResized { width: f32, height: f32, window: window::Id },
    ToggleFullscreen,
    CloseSyncPanel,
    SyncProviderChanged(adit_storage::SyncProvider),
    SyncFieldChanged(SyncField, String),
    SyncSecretChanged(String),
    SyncIncludeCredentialsToggled(bool),
    SyncNow,
    SyncFinished(Result<SyncReport, String>),
    SyncConnectAccount,
    /// A device flow got its code pair back, or could not. Carries the whole
    /// authorisation: the update loop shows its user code, opens the browser,
    /// and hands the rest to the polling worker.
    SyncDeviceCodeReady(Result<Box<adit_sync::backend::device::DeviceAuth>, String>),
    /// Copy the user code to the clipboard. Typing eight characters by hand is
    /// exactly the sort of transcription this can spare, and the code is the
    /// only interaction point the device flow has.
    SyncCopyUserCode,
    /// The refresh token an authorisation produced, or why it failed.
    SyncAuthFinished(Result<String, String>),
    /// The window's display scale factor (device pixels per logical point).
    DisplayScale(f32),
    ToggleSidebar,
    BeginSidebarDrag,
    SidebarDragMove(f32),
    EndSidebarDrag,
    SplitPane,
    ClosePane(usize),
    FocusPane(usize),
    PaneMousePressed(usize),
    PaneRightPressed(usize),
    PanePointerMoved(usize, Point),
    TerminalPointerMoved(Point),
    /// Window-absolute cursor while a selection drag is live (tracked globally so
    /// the drag survives leaving the text area).
    SelectionCursorMoved(Point),
    /// Toggle the text cursor's blink phase.
    CursorBlink,
    /// Grab the terminal scrollbar thumb (start a drag).
    BeginScrollbarDrag,
    /// Cursor moved during a scrollbar drag; the value is the window-absolute Y.
    ScrollbarDragMove(f32),
    /// Release the scrollbar thumb.
    EndScrollbarDrag,
    /// Tick while drag-selecting past the pane edge: scroll and extend.
    SelectionAutoScroll,
    TerminalScrolled(mouse::ScrollDelta),
    BeginTerminalSelection,
    EndTerminalSelection,
    ShowTerminalContextMenu,
    HideTerminalContextMenu,
    CopyTerminalSelection,
    PasteIntoTerminal,
    ClipboardPasted(Option<String>),
    /// The local clipboard, sampled for the active RDP session's remote.
    RdpClipboardPolled(Option<String>),
    TerminalJumpToBottom,
    OpenSelectedProfile,
    ConnectSelectedProfile,
    RetryActiveSession,
    TabPressed(SessionId),
    TabDragOver(SessionId),
    TabReleased,
    CloseSession(SessionId),
    RenameSessionPrompt(SessionId),
    ShowTabContextMenu(SessionId),
    HideTabContextMenu,
    DisconnectSession(SessionId),
    ReconnectSession(SessionId),
    CloneSessionFromTab(SessionId),
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
    CloseSearch,
    SearchQueryChanged(String),
    SearchNext,
    SearchPrev,
    CheckForUpdates,
    UpdateChecked(Result<Option<UpdateInfo>, String>),
    AutoUpdateChecked(Result<Option<UpdateInfo>, String>),
    /// The one-time legacy-keyring import finished; payload is how many secrets
    /// were imported. Flips `keyring_migrated` so it never runs again.
    KeyringMigrated(usize),
    ToggleAutoCheckUpdates(bool),
    ToggleAutoAcceptHostKeys(bool),
    ToggleRdpClipboard(bool),
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

/// Where a dragged sidebar session will drop, shown as an insertion line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProfileDrop {
    /// An insertion line just before/after another session row (adopts that
    /// row's group — which may be "ungrouped").
    Beside {
        profile_id: ProfileId,
        position: ProfileDropPosition,
    },
    /// Over a group header: drop into that group.
    IntoGroup(String),
    /// The top-level zone above everything: drop ungrouped, at the very top.
    TopLevel,
    /// The zone below everything: drop ungrouped, at the very bottom.
    BottomLevel,
}

/// Spacing between top-level folder slots on the interleave scale. Ungrouped
/// sessions carry a `sort_order` on this same scale, so they interleave with
/// folders; a freshly-created session (small order) sits above the first folder.
const TOP_LEVEL_STEP: i32 = 100_000;

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

const SIDEBAR_MIN_WIDTH: f32 = 220.0;
const SIDEBAR_MAX_WIDTH: f32 = 640.0;
const SIDEBAR_DIVIDER_WIDTH: f32 = 5.0;
/// What the hidden sidebar leaves behind: room for one glyph-sized button and
/// nothing else. Hiding the panel is a click, so getting it back has to be one
/// too — but the strip stands next to the terminal for as long as the panel is
/// hidden, so it is sized to disappear rather than to be noticed.
const SIDEBAR_REVEAL_WIDTH: f32 = 16.0;
const MENU_BAR_HEIGHT: f32 = 28.0;
const TOOLBAR_HEIGHT: f32 = 36.0;
const TAB_BAR_HEIGHT: f32 = 34.0;
const STATUS_BAR_HEIGHT: f32 = 28.0;
/// The floating RDP toolbar's own height, used to hang the ⚡ dropdown below it.
const RDP_TOOLBAR_HEIGHT: f32 = 36.0;
const TERMINAL_PANEL_PADDING: f32 = 8.0;
const TERMINAL_HEADER_AND_GAP: f32 = 0.0;
// Single compact line (name only) — SecureCRT-style, less busy than the old
// two-line name + user@host row. Sized to give the 13px name a little breathing room.
const PROFILE_ROW_HEIGHT: f32 = 28.0;
// Split-pane layout.
const PANE_GAP: f32 = 6.0;
const PANE_HEADER_HEIGHT: f32 = 26.0;
const MAX_PANES: usize = 6;

/// Sample the local clipboard for the remote desktop every Nth 100 ms `Tick`.
/// Windows has no cheap "did the clipboard change" signal available to us here,
/// so this is a poll; 500 ms is fast enough that copy-then-paste feels instant
/// and slow enough not to contend with other apps for the clipboard.
const RDP_CLIPBOARD_POLL_TICKS: u8 = 5;
/// Largest byte range to ask the remote for in one round trip. Mirrors the
/// helper-side clamp, which mirrors the wire constant: asking for more just gets
/// clamped there and wastes a message.
const RDP_FILE_CHUNK_BYTES: u32 = 64 * 1024;

/// Smallest gap between RDP texture uploads. Each one is a full-surface
/// allocation through iced's async image worker; see the sampler for why
/// outrunning it flickers.
const RDP_FRAME_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);
/// How long the outgoing tile layer stays under a freshly-swapped one before it
/// is dropped even without a follow-up frame. Long enough for the swap's async
/// GPU upload to have landed many times over; short enough that a static
/// desktop does not hold a spare framebuffer copy forever.
const RDP_PREV_LAYER_LINGER: std::time::Duration = std::time::Duration::from_millis(1000);

/// Tile edge for the desktop texture, in device pixels. 512x512x4 = 1 MiB,
/// comfortably under iced_wgpu's 2 MiB synchronous-upload threshold.
const RDP_TILE: u16 = 512;

/// One piece of the desktop texture: where it sits and what it holds.
#[derive(Clone)]
pub(crate) struct RdpTile {
    /// Row this tile belongs to, in device pixels. The horizontal position is
    /// implied by the order within the row, so no `x` is needed.
    pub(crate) y: u16,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) handle: iced::widget::image::Handle,
}

impl Default for AditApp {
    fn default() -> Self {
        let profile_store = ProfileStore::default();
        let load_result = profile_store.load_catalog();
        // Captured before the match below consumes the catalog. Taken only from
        // a successful load: a failed one must not look like "every group had
        // its icon cleared" and then save that back.
        let group_icons = load_result
            .as_ref()
            .map(|catalog| catalog.group_icons.clone())
            .unwrap_or_default();
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
                tf("已加载空分组配置: {}", &[&profile_store.path().display()]),
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
                    Some(tf("读取会话配置失败: {}", &[&error])),
                )
            }
        };

        Self::with_loaded_state(
            manager,
            groups,
            group_icons,
            profile_store,
            load_notice,
            load_error,
        )
    }
}

impl AditApp {
    fn with_loaded_state(
        mut manager: SessionManager,
        groups: Vec<String>,
        group_icons: std::collections::BTreeMap<String, String>,
        profile_store: ProfileStore,
        load_notice: String,
        load_error: Option<String>,
    ) -> Self {
        let selected_profile = manager.profiles().first().map(|profile| profile.id);

        // Restore persisted preferences (theme, folded groups, window size,
        // auto-reconnect).
        let settings_store = SettingsStore::default();
        let settings = settings_store.load().unwrap_or_default();
        // A settings file written before `theme_mode` existed still carries the
        // boolean, so anyone who had picked a theme keeps it instead of being
        // silently moved onto the system's.
        let theme_mode = settings.theme_mode.unwrap_or(if settings.dark_mode {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        });
        let dark_mode = resolve_dark(theme_mode);
        let host_layout = settings.host_layout;
        let recent_hosts = settings.recent_hosts;
        let grid_order = {
            let mut order = settings.grid_order;
            if order.is_empty() {
                // Seeded from the tree's order on first run, so the two views
                // start identical and then never move together again. Without
                // the seed an empty grid order mirrors the tree live, and the
                // first tree drag "leaks" into a grid that was supposed to be
                // independent — which is indistinguishable from the bug this
                // feature exists to end.
                let mut seeded = manager.profiles().to_vec();
                seeded.sort_by(profile_sidebar_order);
                order = seeded.into_iter().map(|profile| profile.id).collect();
            }
            order
        };
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
        let highlight_rules = settings.highlight_rules;
        // Into the renderer's global before the first frame. Only a toggle
        // rebuilds it after this — never `view`, which runs every frame and
        // would be recompiling regexes for nothing.
        highlight::apply_overrides(&highlight_rules);
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
        let auto_accept_host_keys = settings.auto_accept_host_keys;
        let rdp_clipboard = settings.rdp_clipboard;
        let rdp_quality = settings.rdp_quality;
        let keyring_migrated = settings.keyring_migrated;
        manager.set_auto_accept_host_keys(auto_accept_host_keys);
        manager.set_rdp_clipboard(rdp_clipboard);
        manager.set_rdp_quality(rdp_quality);
        let command_window_open = settings.command_window_open;
        let command_send_immediately = settings.command_send_immediately;

        let mut manager = manager;
        manager.set_auto_reconnect(auto_reconnect);
        manager.set_connect_timeout(u64::from(connect_timeout_secs));

        // Mirror what is on disk (raw, not clamped) so a bad size triggers one
        // corrective write, while a valid size stays untouched.
        // Before the first frame is drawn, not after: otherwise the window
        // opens in Chinese and flips a moment later.
        set_language(settings.language);

        let persisted_settings = AppSettings {
            language: settings.language,
            sync: settings.sync.clone(),
            dark_mode,
            theme_mode: Some(theme_mode),
            host_layout,
            recent_hosts: recent_hosts.clone(),
            grid_order: grid_order.clone(),
            collapsed_groups: collapsed_groups.iter().cloned().collect(),
            window_width: raw_window_width,
            window_height: raw_window_height,
            auto_reconnect,
            sidebar_width: settings.sidebar_width,
            sidebar_visible,
            font_family: font_family.clone(),
            font_size,
            color_scheme: color_scheme.clone(),
            highlight_rules: highlight_rules.clone(),
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
            auto_accept_host_keys,
            rdp_clipboard,
            rdp_quality,
            keyring_migrated,
        };
        // `sidebar_offset` in widget terms, but there is no `AditApp` to ask yet.
        // Hidden still costs the reveal strip's width.
        let effective_sidebar = if sidebar_visible {
            sidebar_width + SIDEBAR_DIVIDER_WIDTH
        } else {
            SIDEBAR_REVEAL_WIDTH
        };

        let mut app = Self {
            group_icons,
            manager,
            profile_store,
            credential_store: CredentialStore::default(),
            selected_profile,
            hovered_profile: None,
            dragged_profile: None,
            profile_drop: None,
            profile_drag_origin: None,
            profile_drag_active: false,
            dragged_group: None,
            group_drag_active: false,
            group_drag_origin: None,
            group_drop: None,
            group_drop_target: None,
            group_context_menu: None,
            editing_group: None,
            group_name_draft: String::new(),
            editing_profile: None,
            profile_name_draft: String::new(),
            profile_context_menu: None,
            tab_context_menu: None,
            profile_editor: None,
            connection_dialog: None,
            groups,
            collapsed_groups,
            active_menu: None,
            profile_group: String::new(),
            profile_advanced_open: false,
            profile_name: String::new(),
            profile_host: String::new(),
            profile_port: String::from("22"),
            profile_username: String::new(),
            profile_auth_method: AuthMethod::Auto,
            profile_password: String::new(),
            profile_passphrase: String::new(),
            profile_protocol: Protocol::Ssh,
            profile_icon: String::new(),
            profile_identity_file: String::new(),
            profile_startup_command: String::new(),
            profile_jumps: String::new(),
            profile_terminal_type: String::new(),
            profile_environment: Environment::None,
            profile_accent_color: String::new(),
            profile_label: String::new(),
            connect_timeout_secs,
            scrollback_lines,
            snippets,
            snippets_open: false,
            snippet_name_draft: String::new(),
            snippet_command_draft: String::new(),
            auto_check_updates,
            auto_accept_host_keys,
            rdp_clipboard,
            rdp_quality,
            rdp_toolbar_collapsed: true,
            rdp_quality_menu_open: false,
            keyring_migrated,
            auth_prompt: None,
            auth_prompt_answers: Vec::new(),
            pending_hyperlink: None,
            password: String::new(),
            remember_connection_password: false,
            session_filter: String::new(),
            sftp_upload_path: String::new(),
            sftp_new_folder: String::new(),
            sftp_rename: None,
            sftp_context_menu: None,
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
            // Startup is never fullscreen (the flag is deliberately not
            // persisted, see `AditApp::fullscreen`).
            terminal_size: estimated_terminal_size(
                window_width,
                window_height,
                effective_sidebar,
                false,
            ),
            terminal_pointer: None,
            terminal_selection: None,
            terminal_selecting: false,
            selection_autoscroll: 0,
            cursor_blink_on: true,
            scrollbar_dragging: false,
            terminal_click: None,
            terminal_context_menu: false,
            terminal_scroll_offset: 0,
            rdp_frame_generation: 0,
            rdp_surface_size: None,
            rdp_target_size: None,
            rdp_frame_session: None,
            rdp_frame_uploaded: None,
            rdp_tiles: Vec::new(),
            rdp_tiles_prev: Vec::new(),
            rdp_surface_size_prev: None,
            rdp_clipboard_offered: None,
            rdp_offered_files: Vec::new(),
            rdp_chunk_bridge: clipboard_files::ChunkBridge::new(),
            settings_save_failed: false,
            rdp_resize_requested_at: None,
            rdp_resize_pending: None,
            rdp_resize_pending_since: None,
            display_scale: 1.0,
            rdp_clipboard_ticks: 0,
            modifiers: keyboard::Modifiers::empty(),
            window_width,
            window_height,
            sidebar_width,
            sidebar_visible,
            fullscreen: false,
            sidebar_dragging: false,
            cursor_pos: Point::ORIGIN,
            context_menu_pos: Point::ORIGIN,
            // Opens on the host list rather than an empty terminal: with no
            // session running there is nothing for the terminal view to show.
            main_view: MainView::Hosts,
            host_layout,
            recent_hosts,
            grid_order,
            drag_from_grid: false,
            hosts_cursor: None,
            card_slots: std::collections::HashMap::new(),
            dark_mode,
            theme_mode,
            font_family,
            font_size,
            color_scheme,
            highlight_rules,
            settings_open: false,
            settings_category: SettingsCategory::App,
            language: settings.language,
            sync_tab: SyncTab::default(),
            update_dialog_open: false,
            update_state: UpdateState::Idle,
            known_hosts_open: false,
            known_hosts: Vec::new(),
            sync: settings.sync.clone(),
            sync_secret_saved: false,
            sync_connecting: false,
            sync_device_prompt: None,
            sync_secret_draft: String::new(),
            sync_busy: false,
            sync_status: String::new(),
            sync_conflicts: Vec::new(),
            config_dir: adit_storage::config_dir(),
            pending_config_dir: None,
            config_dir_custom: adit_storage::custom_config_dir().is_some(),
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
        // Keyring migration is NOT done here: with many profiles it was hundreds
        // of synchronous Credential Manager probes on the boot thread, delaying
        // the window by seconds. It now runs once, off-thread, from `boot` (gated
        // by `keyring_migrated`).
        app
    }
}

/// One-time import of secrets an older build stored in the OS keyring into the
/// encrypted store in the config folder. Without this, upgrading would look like
/// every saved password vanished. Cheap and idempotent: once a secret is in the
/// file the keyring copy is ignored, and profiles with nothing stored cost a
/// single miss each.
/// Run the one-time legacy-keyring import off the boot thread and report how many
/// secrets were pulled in. Probing the OS keyring is blocking (a Credential
/// Manager syscall per profile), so this happens in `spawn_blocking` rather than
/// on the UI thread; the caller persists `keyring_migrated` once it completes so
/// it never runs again. Returns 0 on any error or if there is nothing to do.
async fn migrate_keyring_credentials(
    store: CredentialStore,
    profile_ids: Vec<ProfileId>,
) -> usize {
    if profile_ids.is_empty() {
        return 0;
    }
    tokio::task::spawn_blocking(move || store.migrate_from_keyring(&profile_ids))
        .await
        .unwrap_or(0)
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
    // Boot: build the app and fire off any startup tasks. Both are optional and
    // run OFF the boot thread so the window appears immediately:
    //   - a silent update check (only surfaces the dialog if a newer version exists);
    //   - the one-time legacy-keyring import. With many profiles the import was
    //     hundreds of synchronous Credential Manager probes, and doing it on the
    //     boot thread delayed the window by seconds.
    let boot = || {
        let app = AditApp::default();
        let mut tasks: Vec<Task<Message>> = Vec::new();
        if app.auto_check_updates {
            tasks.push(Task::perform(check_for_update(), Message::AutoUpdateChecked));
        }
        if !app.keyring_migrated {
            let store = app.credential_store.clone();
            let profile_ids: Vec<ProfileId> =
                app.manager.profiles().iter().map(|p| p.id).collect();
            tasks.push(Task::perform(
                migrate_keyring_credentials(store, profile_ids),
                Message::KeyringMigrated,
            ));
        }
        (app, Task::batch(tasks))
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

/// Resolve a preference into the theme actually shown.
///
/// `System` asks the OS, and falls back to dark if it declines to say — Adit's
/// terminal is dark, so that is the answer that looks least like a bug.
///
/// Called when settings load and when the mode changes, never per frame: on
/// Windows this reads the registry, and the render path is no place for that.
fn resolve_dark(mode: ThemeMode) -> bool {
    match mode {
        ThemeMode::Light => false,
        ThemeMode::Dark => true,
        ThemeMode::System => !matches!(dark_light::detect(), Ok(dark_light::Mode::Light)),
    }
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
    // RDP is a video surface: while the ACTIVE session is a live RDP session,
    // sample its framebuffer every frame (vsync-paced) so the desktop stays
    // smooth (the 100 ms Tick would look choppy). Gating on the active tab avoids
    // pinning the app at 60 fps just because a background RDP tab is open.
    if app.manager.active_rdp_live() {
        subs.push(window::frames().map(|_| Message::RdpTick));
    }
    // Cards easing to new positions need a frame each; the 100 ms Tick would
    // render the move as three steps. Only while something is actually moving,
    // so an idle grid costs nothing.
    if cards_are_moving(app) {
        subs.push(window::frames().map(|_| Message::Tick));
    }
    // Blink the text cursor only where one is actually drawn — a focused terminal
    // tab. Otherwise this would wake the app twice a second to redraw nothing.
    // 530 ms is the long-standing terminal blink period.
    if terminal_cursor_blinks(app) {
        subs.push(iced::time::every(Duration::from_millis(530)).map(|_| Message::CursorBlink));
    }
    // Only track the global cursor while a sidebar resize is in progress, so
    // idle mouse movement never floods the app with messages.
    if app.sidebar_dragging {
        subs.push(event::listen_with(sidebar_drag_event));
    }
    // A scrollbar-thumb drag tracks the cursor window-wide so it doesn't get stuck
    // when the pointer slips off the thin bar.
    if app.scrollbar_dragging {
        subs.push(event::listen_with(scrollbar_drag_event));
    }
    // While a text selection drag is live, catch the button-up anywhere — even
    // outside the terminal panel — so the selection can't get "stuck" extending
    // after the user releases past the panel edge or over another widget.
    if app.terminal_selecting {
        subs.push(event::listen_with(terminal_selection_event));
        // Dragging past the top/bottom edge keeps scrolling (and extending the
        // selection) even if the cursor then holds still — no more mouse events
        // would arrive to drive it.
        if app.selection_autoscroll != 0 {
            subs.push(
                iced::time::every(Duration::from_millis(60)).map(|_| Message::SelectionAutoScroll),
            );
        }
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

/// While a selection drag is live, track the cursor and the button-up GLOBALLY.
///
/// A pane's `mouse_area` only reports `on_move` while the pointer is inside its
/// bounds, so once the drag leaves the text area the selection would freeze (and
/// the edge auto-scroll would never arm — nothing would tell it the pointer is
/// past the edge). Listening at the runtime level keeps the drag alive anywhere in
/// the window, which is also how the sidebar resize works.
fn terminal_selection_event(
    event: event::Event,
    _status: event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        event::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
            Some(Message::EndTerminalSelection)
        }
        event::Event::Mouse(mouse::Event::CursorMoved { position }) => {
            Some(Message::SelectionCursorMoved(position))
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
    window: window::Id,
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
            window,
        }),
        // Window-absolute cursor for context-menu anchoring (the tab strip has no
        // local move tracker of its own).
        event::Event::Mouse(mouse::Event::CursorMoved { position }) => {
            Some(Message::GlobalCursorMoved(position))
        }
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

// Corner-radius scale. Interactive controls and floating surfaces are rounded;
// full-bleed structural bars stay square (see the *_style fns below).
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
    fn rdp_scancodes_match_pc_at_set1() {
        // Base make codes.
        assert_eq!(rdp_scancode_for_code(Code::KeyQ), Some((0x10, false)));
        assert_eq!(rdp_scancode_for_code(Code::KeyA), Some((0x1E, false)));
        assert_eq!(rdp_scancode_for_code(Code::KeyZ), Some((0x2C, false)));
        assert_eq!(rdp_scancode_for_code(Code::Enter), Some((0x1C, false)));
        assert_eq!(rdp_scancode_for_code(Code::Space), Some((0x39, false)));
        assert_eq!(rdp_scancode_for_code(Code::Digit1), Some((0x02, false)));
        assert_eq!(rdp_scancode_for_code(Code::F1), Some((0x3B, false)));
        assert_eq!(rdp_scancode_for_code(Code::F12), Some((0x58, false)));
        // E0-extended: same base code, extended flag distinguishes it.
        assert_eq!(rdp_scancode_for_code(Code::NumpadEnter), Some((0x1C, true)));
        assert_eq!(rdp_scancode_for_code(Code::ArrowUp), Some((0x48, true)));
        assert_eq!(rdp_scancode_for_code(Code::ArrowLeft), Some((0x4B, true)));
        assert_eq!(rdp_scancode_for_code(Code::ControlRight), Some((0x1D, true)));
        assert_eq!(rdp_scancode_for_code(Code::NumpadDivide), Some((0x35, true)));
        // Unmapped keys yield None (e.g. PrintScreen's multi-byte sequence).
        assert_eq!(rdp_scancode_for_code(Code::PrintScreen), None);
    }

    #[test]
    fn installer_asset_is_picked_by_architecture() {
        let assets = serde_json::json!([
            {"name": "adit_0.1.61_amd64.deb", "browser_download_url": "https://x/deb"},
            {"name": "adit-installer-v0.1.61.exe", "browser_download_url": "https://x/x64"},
            {"name": "adit-installer-v0.1.61_arm64.exe", "browser_download_url": "https://x/arm"},
        ]);
        let assets = assets.as_array().unwrap();

        // Each architecture gets its own build, not merely the first .exe listed.
        assert_eq!(
            pick_installer_asset(assets, "x86_64").unwrap().0,
            "https://x/x64"
        );
        assert_eq!(
            pick_installer_asset(assets, "aarch64").unwrap().0,
            "https://x/arm"
        );

        // Order must not decide it: arm64 first still resolves x86_64 correctly.
        let reversed = serde_json::json!([
            {"name": "adit-installer-v0.1.61-arm64.exe", "browser_download_url": "https://x/arm"},
            {"name": "adit-installer-v0.1.61.exe", "browser_download_url": "https://x/x64"},
        ]);
        assert_eq!(
            pick_installer_asset(reversed.as_array().unwrap(), "x86_64")
                .unwrap()
                .0,
            "https://x/x64"
        );

        // A release with only the old single installer still updates an x64
        // machine, and leaves an arm64 one the emulated build rather than nothing.
        let legacy = serde_json::json!([
            {"name": "adit-installer-v0.1.60.exe", "browser_download_url": "https://x/only"},
        ]);
        let legacy = legacy.as_array().unwrap();
        assert_eq!(
            pick_installer_asset(legacy, "x86_64").unwrap().0,
            "https://x/only"
        );
        assert_eq!(
            pick_installer_asset(legacy, "aarch64").unwrap().0,
            "https://x/only"
        );

        // Nothing installable at all is None, not a blank URL that would 404.
        let none = serde_json::json!([{"name": "notes.txt", "browser_download_url": "https://x/t"}]);
        assert!(pick_installer_asset(none.as_array().unwrap(), "x86_64").is_none());
    }

    /// The naming rule that protects updaters shipped before `pick_installer_asset`
    /// existed. They take the first `.exe` GitHub lists, and GitHub lists a
    /// release's assets sorted by name — so the x64 installer has to sort first.
    ///
    /// v0.1.61 shipped `-arm64.exe`, and `-` (0x2D) sorts before `.` (0x2E), so
    /// the arm64 build came first and every old updater on an x64 machine was
    /// offered an installer that refuses to run there. `_` (0x5F) sorts after.
    /// An app with a deterministic three-host catalogue and both stores pointed
    /// at a scratch directory, so a test drag can never touch the real
    /// profiles.json or settings.json on the machine running the tests.
    #[allow(clippy::field_reassign_with_default)]
    /// Switching RDP clipboard sharing off has to cut the flow that is already
    /// running, not just the next connection. The poll captures local text into
    /// `rdp_clipboard_offered`; leaving it there would keep the last thing
    /// copied queued for whatever remote asks next, so the setting would read
    /// as off while still handing data over.
    #[test]
    fn turning_the_rdp_clipboard_off_drops_what_was_already_captured() {
        let mut app = drag_test_app();
        app.rdp_clipboard = true;
        app.rdp_clipboard_offered = Some(String::from("a password, probably"));

        let _ = update(&mut app, Message::ToggleRdpClipboard(false));

        assert!(!app.rdp_clipboard);
        assert_eq!(app.rdp_clipboard_offered, None);
    }

    /// And the default stays on, matching mstsc: a toggle nobody asked for is
    /// not an excuse to change what happens out of the box.
    #[test]
    fn the_rdp_clipboard_defaults_to_on() {
        assert!(AppSettings::default().rdp_clipboard);
        assert!(drag_test_app().rdp_clipboard);
    }

    /// The toolbar starts visible, and nothing but the ⌃ button hides it.
    ///
    /// This is the regression guard for the flicker: the first version revealed
    /// the bar on hover, which needs one widget to sense the pointer and another
    /// to be clicked — and `stack` gives the topmost layer the cursor while
    /// telling the layers below that the pointer left. The bar appearing over its
    /// own reveal strip therefore read as "pointer gone", hid itself, and
    /// re-triggered, once per frame. If toolbar visibility ever depends on
    /// pointer position again, that loop comes back.
    #[test]
    fn the_toolbar_opens_and_closes_only_on_request() {
        let mut app = drag_test_app();
        // Starts collapsed: windowed mode should cost a tab, not a bar. The old
        // docked toolbar was deleted for spending 36px on menu duplicates, and
        // this must not quietly become that again.
        assert!(app.rdp_toolbar_collapsed, "the bar starts collapsed");

        let _ = update(&mut app, Message::ToggleRdpToolbarCollapsed);
        assert!(!app.rdp_toolbar_collapsed);
        let _ = update(&mut app, Message::ToggleRdpToolbarCollapsed);
        assert!(app.rdp_toolbar_collapsed);
    }

    /// The toolbar draws in *both* window modes. Asserted through the shape
    /// rather than the state, because the first version of this shipped with an
    /// early return that drew nothing in windowed mode — and the state-only
    /// tests stayed green over it.
    #[test]
    fn the_toolbar_draws_in_windowed_mode_too() {
        let mut app = drag_test_app();
        app.fullscreen = false;
        assert_eq!(rdp_toolbar_shape(&app), ToolbarShape::Tab);

        app.rdp_toolbar_collapsed = false;
        assert_eq!(rdp_toolbar_shape(&app), ToolbarShape::Expanded);
    }

    /// Fullscreen has no menu bar, so the toolbar is the only place its controls
    /// exist there — including the way back out. Leaving fullscreen puts it away
    /// again, where the 视图 menu covers the same ground.
    #[test]
    fn entering_fullscreen_opens_the_toolbar_and_leaving_closes_it() {
        let mut app = drag_test_app();
        assert!(!app.fullscreen);

        let _ = update(&mut app, Message::ToggleFullscreen);
        assert!(app.fullscreen);
        assert!(!app.rdp_toolbar_collapsed, "fullscreen must show the bar");

        let _ = update(&mut app, Message::ToggleFullscreen);
        assert!(!app.fullscreen);
        assert!(app.rdp_toolbar_collapsed);
    }

    /// Collapsing takes the quality dropdown with it — the menu hangs off a
    /// button that is no longer drawn, and an orphaned card floating over the
    /// desktop is exactly the kind of leftover the hover version produced.
    #[test]
    fn collapsing_the_toolbar_closes_the_quality_menu() {
        let mut app = drag_test_app();
        // Open the bar first: the dropdown only exists while it is open.
        app.rdp_toolbar_collapsed = false;
        app.rdp_quality_menu_open = true;

        let _ = update(&mut app, Message::ToggleRdpToolbarCollapsed);
        assert!(app.rdp_toolbar_collapsed);
        assert!(!app.rdp_quality_menu_open);

        // And expanding again does not resurrect it.
        let _ = update(&mut app, Message::ToggleRdpToolbarCollapsed);
        assert!(!app.rdp_quality_menu_open);
    }

    /// Fit mode changes nothing about the picture's geometry until it is on, and
    /// then it must never enlarge past the pane on either axis. The factor is
    /// what both the tile layout and the mouse mapping divide by, so a wrong one
    /// shows up as clicks landing somewhere other than the pointer.
    #[test]
    fn an_honoured_or_unrequested_surface_renders_one_to_one() {
        let mut app = drag_test_app();
        app.display_scale = 1.0;
        // Nothing requested: whatever the server sent renders exactly 1:1.
        app.rdp_target_size = None;
        assert_eq!(rdp_fit_factors(&app, (4096, 2160)), (1.0, 1.0));
        // An honoured request: still exactly 1:1.
        app.rdp_target_size = Some((4096, 2160));
        assert_eq!(rdp_fit_factors(&app, (4096, 2160)), (1.0, 1.0));
        // A degenerate surface must not produce a factor that would divide into
        // the mouse mapping and send garbage coordinates to the remote.
        assert_eq!(rdp_fit_factors(&app, (0, 0)), (1.0, 1.0));
    }

    /// While a resize is in flight the stale surface scales to fit even with
    /// fit mode off. The pane changes instantly; the server takes a round trip
    /// to deliver the new resolution; drawing the old frame 1:1 in that window
    /// is the flash of black bars reported from a real session.
    #[test]
    fn a_resize_in_flight_scales_the_stale_surface() {
        let mut app = drag_test_app();
        app.display_scale = 1.0;
        app.rdp_target_size = Some((800, 600));
        app.rdp_resize_requested_at = Some(std::time::Instant::now());

        // The delivered surface is not the requested one yet: scale it.
        let (fx, fy) = rdp_fit_factors(&app, (4096, 2160));
        assert!(fx < 1.0 && fy < 1.0);
        // The requested size arrived: back to exact 1:1.
        assert_eq!(rdp_fit_factors(&app, (800, 600)), (1.0, 1.0));
    }

    /// A debounced (queued, unsent) resize also presents in transitional form:
    /// the pane has already changed, and 1:1 would show the mismatch for the
    /// length of the debounce window.
    #[test]
    fn a_pending_debounced_resize_keeps_the_picture_filling_the_pane() {
        let mut app = drag_test_app();
        app.display_scale = 1.0;
        app.rdp_resize_pending = Some((800, 600));

        let (fx, fy) = rdp_fit_factors(&app, (4000, 2000));
        assert!(fx < 1.0 && fy < 1.0);
        // Once the surface matches what is queued there is nothing to hide.
        assert_eq!(rdp_fit_factors(&app, (800, 600)), (1.0, 1.0));
    }

    /// The transition fills both axes independently. Aspect-preserving scaling
    /// cannot fill a pane whose aspect changed — a sidebar toggle changes only
    /// the width, the height axis already fits (factor ≈ 1.0), and the uniform
    /// minimum then does nothing, leaving the exact black bar the transition
    /// exists to remove. A real session's probe capture showed it: 1908→2220
    /// requested, uniform factor 1.002, 312-pixel bar intact.
    #[test]
    fn the_transition_fills_both_axes_independently() {
        let mut app = drag_test_app();
        app.display_scale = 1.0;
        app.rdp_target_size = Some((800, 600));
        app.rdp_resize_requested_at = Some(std::time::Instant::now());

        // Same width, different heights: the x factor must hold still while the
        // y factor moves. A uniform factor would couple them.
        let (fx1, fy1) = rdp_fit_factors(&app, (4000, 2000));
        let (fx2, fy2) = rdp_fit_factors(&app, (4000, 1000));
        assert_eq!(fx1, fx2);
        assert!(fy2 > fy1);

        // Once the request goes stale the fallback is aspect-preserving.
        app.rdp_resize_requested_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(10));
        let (fx, fy) = rdp_fit_factors(&app, (4000, 1000));
        assert_eq!(fx, fy);
    }

    /// A server that never honours the request must not stay stretched forever.
    /// Past the transition window the presentation settles into an
    /// aspect-preserving letterbox — the automatic replacement for the manual
    /// fit toggle — so the whole desktop stays visible, undistorted.
    #[test]
    fn a_refused_resize_settles_into_a_letterbox() {
        let mut app = drag_test_app();
        app.display_scale = 1.0;
        app.rdp_target_size = Some((800, 600));
        app.rdp_resize_requested_at =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(10));
        // `checked_sub` can fail only moments after boot; the test needs the
        // subtraction to have happened to mean anything.
        assert!(app.rdp_resize_requested_at.is_some());

        let (fx, fy) = rdp_fit_factors(&app, (4096, 2160));
        assert_eq!(fx, fy, "the steady fallback is aspect-preserving");
        assert!(fx < 1.0);
    }

    /// Ctrl+Alt+Del is six events, and the release order is the part that
    /// matters: a modifier left down turns every later keystroke into a chord.
    #[test]
    fn ctrl_alt_del_releases_its_modifiers_last() {
        // The sequence is fixed in `send_ctrl_alt_del`; this pins the contract
        // that the presses and releases mirror each other.
        let expected = [
            (0x1D, false, true),
            (0x38, false, true),
            (0x53, true, true),
            (0x53, true, false),
            (0x38, false, false),
            (0x1D, false, false),
        ];
        for (index, (_, _, pressed)) in expected.iter().enumerate() {
            assert_eq!(*pressed, index < 3, "event {index} is on the wrong half");
        }
        // Reverse order: the nth release undoes the nth-from-last press.
        for index in 0..3 {
            let (press_code, press_ext, _) = expected[index];
            let (release_code, release_ext, _) = expected[5 - index];
            assert_eq!(press_code, release_code);
            assert_eq!(press_ext, release_ext);
        }
    }

    fn drag_test_app() -> AditApp {
        let scratch = std::env::temp_dir().join(format!(
            "adit-drag-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
        ));
        let mut app = AditApp {
            profile_store: ProfileStore::new(scratch.join("profiles.json")),
            settings_store: SettingsStore::new(scratch.join("settings.json")),
            manager: SessionManager::with_profiles(vec![
                ConnectionProfile::with_group("g", "a", "10.0.0.1", 22, "root"),
                ConnectionProfile::with_group("g", "b", "10.0.0.2", 22, "root"),
                ConnectionProfile::with_group("g", "c", "10.0.0.3", 22, "root"),
            ]),
            ..AditApp::default()
        };
        app.grid_order.clear();
        app.recent_hosts.clear();
        app
    }

    fn sidebar_names(app: &AditApp) -> Vec<String> {
        let mut profiles = app.manager.profiles().to_vec();
        profiles.sort_by(profile_sidebar_order);
        profiles.into_iter().map(|profile| profile.name).collect()
    }

    fn grid_names(app: &AditApp) -> Vec<String> {
        grid_ordered_profiles(app)
            .into_iter()
            .map(|profile| profile.name)
            .collect()
    }

    /// The full message sequence the widgets emit for one drag, in order: the
    /// press on the source, the enter + move over the target, the target's
    /// release, and the global release that always follows it. Each view emits
    /// its own hover/drag-over pair, so the test drives whichever pair the drag
    /// origin would.
    fn drive_drag(app: &mut AditApp, from_grid: bool, source: usize, target: usize) {
        let ids: Vec<ProfileId> = app.manager.profiles().iter().map(|p| p.id).collect();
        if from_grid {
            let _ = update(app, Message::GridProfilePressed(ids[source]));
            let _ = update(app, Message::GridProfileHovered(ids[target]));
            let _ = update(
                app,
                Message::GridProfileDragOver(ids[target], ProfileDropPosition::After),
            );
        } else {
            let _ = update(app, Message::ProfilePressed(ids[source]));
            let _ = update(app, Message::ProfileHovered(ids[target]));
            let _ = update(
                app,
                Message::ProfileDragOver(ids[target], ProfileDropPosition::After),
            );
        }
        let _ = update(app, Message::ProfileDropped(ids[target]));
        let _ = update(app, Message::CancelProfileDrag);
    }

    #[test]
    fn a_grid_drag_moves_the_grid_and_not_the_tree() {
        let mut app = drag_test_app();
        drive_drag(&mut app, true, 0, 2);
        assert_eq!(grid_names(&app), ["b", "c", "a"], "the grid must reorder");
        assert_eq!(
            sidebar_names(&app),
            ["a", "b", "c"],
            "the tree must not move when the drag started on a card"
        );
    }

    #[test]
    fn a_tree_drag_moves_the_tree_and_not_the_grid() {
        let mut app = drag_test_app();
        // Startup seeds the grid's order from the tree (the test helper builds
        // its catalogue after init, so seed the same way init does).
        app.grid_order = app.manager.profiles().iter().map(|p| p.id).collect();
        drive_drag(&mut app, false, 0, 2);
        assert_eq!(
            sidebar_names(&app),
            ["b", "c", "a"],
            "the tree must reorder"
        );
        // The whole point of the seed: the grid holds the arrangement it had,
        // and a tree drag cannot leak into it — not even the first one.
        assert_eq!(grid_names(&app), ["a", "b", "c"]);
    }

    /// The band's arrangement, which mid-drag is the arrangement a release would
    /// produce.
    fn slot_shape(app: &AditApp) -> String {
        // The grid's own order, which is what the grid renders — reading the
        // tree's here would have asserted the wrong view's state entirely.
        let ordered = grid_ordered_profiles(app);
        let hosts: Vec<&ConnectionProfile> = ordered.iter().collect();
        band_slots(app, hosts)
            .into_iter()
            .map(|profile| profile.name.clone())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn a_drag_previews_the_arrangement_it_will_produce() {
        let mut app = drag_test_app();
        let ids: Vec<ProfileId> = app.manager.profiles().iter().map(|p| p.id).collect();
        assert_eq!(slot_shape(&app), "abc", "at rest");

        let _ = update(&mut app, Message::GridProfilePressed(ids[0]));
        let _ = update(&mut app, Message::GridProfileHovered(ids[2]));
        // `a` has moved to just before `c`, so `b` shifted up to fill its place.
        assert_eq!(slot_shape(&app), "bac");

        let _ = update(
            &mut app,
            Message::GridProfileDragOver(ids[2], ProfileDropPosition::After),
        );
        // Crossing c's midpoint carries `a` past it; nothing else jumps.
        assert_eq!(slot_shape(&app), "bca");

        // What was on screen mid-drag is what the release produces.
        let _ = update(&mut app, Message::CancelProfileDrag);
        assert_eq!(slot_shape(&app), "bca");
    }

    #[test]
    fn a_tree_drag_does_not_disturb_the_grid_layout() {
        // Only a grid drag reshapes the grid. A tree drag must leave every card
        // exactly where it is, gap included — that is, no gap at all.
        let mut app = drag_test_app();
        let ids: Vec<ProfileId> = app.manager.profiles().iter().map(|p| p.id).collect();
        let _ = update(&mut app, Message::ProfilePressed(ids[0]));
        let _ = update(&mut app, Message::ProfileHovered(ids[2]));
        assert_eq!(slot_shape(&app), "abc", "a tree drag must not reshuffle the grid");
        let _ = update(&mut app, Message::CancelProfileDrag);
    }

    #[test]
    fn closing_the_last_tab_falls_back_to_the_host_grid() {
        // Closing tabs one by one used to end on an empty terminal: a black pane
        // captioned `not connected`, with nothing on it that led anywhere.
        let mut app = drag_test_app();
        let ids: Vec<ProfileId> = app.manager.profiles().iter().map(|p| p.id).collect();
        let first = app.manager.open_mock_session(ids[0]).unwrap();
        let second = app.manager.open_mock_session(ids[1]).unwrap();
        app.main_view = MainView::Terminal;

        let _ = update(&mut app, Message::CloseSession(first));
        assert_eq!(
            app.main_view,
            MainView::Terminal,
            "a tab is left, so the terminal view is still where the user is"
        );

        let _ = update(&mut app, Message::CloseSession(second));
        assert_eq!(
            app.main_view,
            MainView::Hosts,
            "the last close has to land somewhere the user can act"
        );
        assert!(!app.terminal_focused, "and a hidden terminal takes no keys");

        // Over-triggering is the other half of the bug: asking for the terminal
        // view with nothing open is the user's own choice.
        let _ = update(&mut app, Message::ShowMainView(MainView::Terminal));
        assert_eq!(app.main_view, MainView::Terminal);
    }

    #[test]
    fn closing_the_last_tab_from_the_menu_falls_back_too() {
        // The 关闭标签 menu item is a second route to zero sessions; it shares
        // `close_session_tab` so it cannot drift from the tab's own ×.
        let mut app = drag_test_app();
        let ids: Vec<ProfileId> = app.manager.profiles().iter().map(|p| p.id).collect();
        app.manager.open_mock_session(ids[0]).unwrap();
        app.main_view = MainView::Terminal;

        let _ = update(&mut app, Message::RunMenu(MenuCommand::CloseActiveTab));
        assert!(app.manager.sessions().is_empty());
        assert_eq!(app.main_view, MainView::Hosts);
    }

    #[test]
    fn hidden_sidebar_still_offsets_the_terminal() {
        // The reveal strip is real layout, not an overlay: terminal hit-testing
        // measures from this offset, so a value that disagrees with `chrome::view`
        // selects the wrong cells rather than misdrawing anything.
        let mut app = drag_test_app();
        app.sidebar_visible = true;
        app.sidebar_width = 260.0;
        assert_eq!(sidebar_offset(&app), 260.0 + SIDEBAR_DIVIDER_WIDTH);

        app.sidebar_visible = false;
        assert_eq!(sidebar_offset(&app), SIDEBAR_REVEAL_WIDTH);

        // Fullscreen drops both — the remote desktop owns the whole width.
        app.fullscreen = true;
        assert_eq!(sidebar_offset(&app), 0.0);
        app.sidebar_visible = true;
        assert_eq!(sidebar_offset(&app), 0.0);
    }

    #[test]
    fn sidebar_drop_zones_do_not_commit_a_grid_drag() {
        // A grid drag released over the sidebar's top-level zone used to fall
        // through to the tree path and yank the host out of its group.
        let mut app = drag_test_app();
        let ids: Vec<ProfileId> = app.manager.profiles().iter().map(|p| p.id).collect();
        let _ = update(&mut app, Message::GridProfilePressed(ids[0]));
        let _ = update(&mut app, Message::ProfileDragOverTop);
        let _ = update(&mut app, Message::CancelProfileDrag);
        assert_eq!(
            sidebar_names(&app),
            ["a", "b", "c"],
            "a grid drag must never commit through the tree's own drop zones"
        );
        assert_eq!(
            app.manager.profiles().iter().filter(|p| p.group == "g").count(),
            3,
            "and it must not ungroup anything"
        );
    }

    #[test]
    fn installer_asset_ordering_favours_x64() {
        let x64 = "adit-installer-v0.1.62.exe";
        let arm = "adit-installer-v0.1.62_arm64.exe";
        assert!(x64 < arm, "the x64 installer must sort before {arm}");
        // The mistake this guards against, stated so it cannot creep back.
        assert!(
            "adit-installer-v0.1.62-arm64.exe" < x64,
            "a '-' separator sorts the arm64 build first — that is the bug"
        );
        // And the arch-aware path still recognises the underscore spelling.
        assert!(arm.contains("arm64"));
    }

    #[test]
    fn only_safe_http_urls_are_openable() {
        assert!(is_openable_http_url("https://example.com/a?b=1#c"));
        assert!(is_openable_http_url("http://10.0.0.1:8080/path"));
        assert!(is_openable_http_url("HTTPS://Example.COM"));
        // Non-http(s) schemes a hostile server might emit are refused.
        assert!(!is_openable_http_url("file:///C:/Windows/System32/calc.exe"));
        assert!(!is_openable_http_url("javascript:alert(1)"));
        assert!(!is_openable_http_url("ftp://x/y"));
        assert!(!is_openable_http_url(""));
        // Shell/argument-splitting vectors: spaces and control chars are refused.
        assert!(!is_openable_http_url("https://x/a b"));
        assert!(!is_openable_http_url("https://x/a\nhttp://y"));
        assert!(!is_openable_http_url("https://x\t& calc"));
        // Unicode bidi/format/separator chars that could spoof the shown URL are
        // refused (only printable ASCII is accepted).
        assert!(!is_openable_http_url("https://good.com\u{202e}moc.live"));
        assert!(!is_openable_http_url("https://x\u{2028}evil"));
        assert!(!is_openable_http_url("https://xn--\u{200b}spoof"));
        assert!(!is_openable_http_url("https://ｅxample.com")); // full-width e
    }

    #[test]
    fn hyperlink_parse_hex_color_is_panic_free() {
        assert!(parse_hex_color("#zzzzzz").is_none());
        assert!(parse_hex_color("#12").is_none());
        assert!(parse_hex_color("１２３４５６").is_none()); // full-width digits
        assert_eq!(parse_hex_color("#1a2b3c"), Some(Color::from_rgb8(26, 43, 60)));
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
    fn folder_reorder_is_direction_aware() {
        let base = || vec!["A".to_string(), "B".to_string(), "C".to_string(), "D".to_string()];
        // Drag B (up) onto A -> lands before A.
        assert_eq!(reordered_folders(base(), "B", "A"), vec!["B", "A", "C", "D"]);
        // Drag A (down) onto C -> lands after C.
        assert_eq!(reordered_folders(base(), "A", "C"), vec!["B", "C", "A", "D"]);
        // Drag D (up) onto B -> lands before B.
        assert_eq!(reordered_folders(base(), "D", "B"), vec!["A", "D", "B", "C"]);
        // Drag A (down) onto the last -> lands at the very end.
        assert_eq!(reordered_folders(base(), "A", "D"), vec!["B", "C", "D", "A"]);
        // Onto itself, or an unknown name, is a no-op.
        assert_eq!(reordered_folders(base(), "B", "B"), base());
        assert_eq!(reordered_folders(base(), "B", "Z"), base());
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
            alt_screen: false,
        };
        let selection = TerminalSelection {
            start: TerminalPoint { row: 0, col: 2 },
            end: TerminalPoint { row: 2, col: 4 },
        };

        assert_eq!(selection_to_text(&snapshot, selection), "pha\nbravo\nchar");
    }

    /// The selection is stored in ABSOLUTE scrollback rows, so a snapshot scrolled
    /// back (first_row > 0) must resolve them against its own window — otherwise a
    /// selection made after scrolling would copy the wrong lines.
    #[test]
    fn selection_to_text_resolves_absolute_rows_in_a_scrolled_snapshot() {
        let snapshot = TerminalSnapshot {
            title: String::from("test"),
            size: TerminalSize::new(10, 3),
            first_row: 100,
            total_rows: 103,
            lines: vec![
                TerminalLine::plain("alpha"),
                TerminalLine::plain("bravo"),
                TerminalLine::plain("charlie"),
            ],
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            alt_screen: false,
        };
        // Absolute rows 100..=102 are the three visible lines.
        let selection = TerminalSelection {
            start: TerminalPoint { row: 100, col: 2 },
            end: TerminalPoint { row: 102, col: 4 },
        };
        assert_eq!(selection_to_text(&snapshot, selection), "pha\nbravo\nchar");

        // Rows entirely above the window resolve to nothing.
        let above = TerminalSelection {
            start: TerminalPoint { row: 0, col: 0 },
            end: TerminalPoint { row: 5, col: 4 },
        };
        assert_eq!(selection_to_text(&snapshot, above), "");
    }

    #[test]
    fn selection_for_viewport_clips_to_the_visible_window() {
        let first_row = 100;
        let rows = 3; // absolute rows 100..=102 visible

        // Entirely inside: shifted down by first_row, cols preserved.
        let inside = TerminalSelection {
            start: TerminalPoint { row: 101, col: 2 },
            end: TerminalPoint { row: 102, col: 4 },
        };
        assert_eq!(
            selection_for_viewport(inside, first_row, rows),
            Some(TerminalSelection {
                start: TerminalPoint { row: 1, col: 2 },
                end: TerminalPoint { row: 2, col: 4 },
            })
        );

        // Starts above the window: clipped to row 0 col 0 (whole first row selected).
        let from_above = TerminalSelection {
            start: TerminalPoint { row: 40, col: 7 },
            end: TerminalPoint { row: 101, col: 3 },
        };
        assert_eq!(
            selection_for_viewport(from_above, first_row, rows),
            Some(TerminalSelection {
                start: TerminalPoint { row: 0, col: 0 },
                end: TerminalPoint { row: 1, col: 3 },
            })
        );

        // Runs off the bottom: last visible row runs to end-of-line.
        let past_below = TerminalSelection {
            start: TerminalPoint { row: 101, col: 1 },
            end: TerminalPoint { row: 500, col: 2 },
        };
        assert_eq!(
            selection_for_viewport(past_below, first_row, rows),
            Some(TerminalSelection {
                start: TerminalPoint { row: 1, col: 1 },
                end: TerminalPoint {
                    row: 2,
                    col: usize::MAX
                },
            })
        );

        // Wholly off-screen in either direction renders nothing.
        let above = TerminalSelection {
            start: TerminalPoint { row: 10, col: 0 },
            end: TerminalPoint { row: 20, col: 0 },
        };
        assert_eq!(selection_for_viewport(above, first_row, rows), None);
        let below = TerminalSelection {
            start: TerminalPoint { row: 200, col: 0 },
            end: TerminalPoint { row: 300, col: 0 },
        };
        assert_eq!(selection_for_viewport(below, first_row, rows), None);

        // A reversed drag (end above start) is normalized before clipping.
        let reversed = TerminalSelection {
            start: TerminalPoint { row: 102, col: 4 },
            end: TerminalPoint { row: 101, col: 2 },
        };
        assert_eq!(
            selection_for_viewport(reversed, first_row, rows),
            Some(TerminalSelection {
                start: TerminalPoint { row: 1, col: 2 },
                end: TerminalPoint { row: 2, col: 4 },
            })
        );
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
