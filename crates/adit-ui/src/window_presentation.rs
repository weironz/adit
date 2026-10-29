//! Window-local presentation only. No protocol handle, credential store or
//! SessionManager is cloned or stored here.
use super::*;

macro_rules! presentation {
    ($($field:ident: $ty:ty = $initial:expr),* $(,)?) => {
        pub(crate) struct WindowPresentation { $(pub $field: $ty,)* }
        impl Default for WindowPresentation {
            fn default() -> Self { Self { $($field: $initial,)* } }
        }
        impl WindowPresentation {
            // iced updates are serialized on the UI thread. Project the event's
            // window onto the existing handlers, then restore the primary view
            // before returning a Task. Async messages retain their window ID.
            pub fn swap(&mut self, app: &mut AditApp) {
                $(std::mem::swap(&mut self.$field, &mut app.$field);)*
            }
        }
    }
}

presentation! {
    active_menu: Option<MenuKind> = None,
    profile_context_menu: Option<ProfileId> = None,
    tab_context_menu: Option<SessionId> = None,
    group_context_menu: Option<String> = None,
    move_to_group_menu: bool = false,
    profile_editor: Option<ProfileId> = None,
    settings_open: bool = false,
    snippets_open: bool = false,
    tunnels_open: bool = false,
    about_open: bool = false,
    update_dialog_open: bool = false,
    known_hosts_open: bool = false,
    editing_group: Option<String> = None,
    group_name_draft: String = String::new(),
    editing_profile: Option<ProfileId> = None,
    profile_name_draft: String = String::new(),
    renaming_session: Option<SessionId> = None,
    session_rename_draft: String = String::new(),
    dragged_tab: Option<SessionId> = None,
    pending_hyperlink: Option<String> = None,
    hyperlink_prompt_generation: u64 = 0,
    terminal_input: String = String::new(),
    terminal_focused: bool = true,
    accessibility_focus_restore: Option<accessibility::RemoteFocusLease> = None,
    terminal_size: TerminalSize = TerminalSize::default(),
    terminal_pointer: Option<TerminalPoint> = None,
    terminal_selection: Option<TerminalSelection> = None,
    terminal_selecting: bool = false,
    selection_autoscroll: i32 = 0,
    cursor_blink_on: bool = true,
    scrollbar_dragging: bool = false,
    terminal_click: Option<(TerminalPoint, Instant, u8)> = None,
    terminal_context_menu: bool = false,
    terminal_scroll_offset: usize = 0,
    rdp_frame_generation: u64 = 0,
    rdp_surface_size: Option<(u16, u16)> = None,
    rdp_target_size: Option<(u16, u16)> = None,
    rdp_frame_session: Option<SessionId> = None,
    rdp_frame_uploaded: Option<Instant> = None,
    rdp_tiles: Vec<RdpTile> = Vec::new(),
    rdp_tiles_prev: Vec<RdpTile> = Vec::new(),
    rdp_surface_size_prev: Option<(u16, u16)> = None,
    vnc_frame_coverage: update_loop::VncFrameCoverage = Default::default(),
    rdp_clipboard_offered: Option<String> = None,
    rdp_clipboard_seq: u32 = 0,
    rdp_image_offered_seq: u32 = 0,
    rdp_clipboard_ticks: u8 = 0,
    desktop_clipboard_epoch: u64 = 0,
    rdp_offered_files: Vec<clipboard_files::OfferedFile> = Vec::new(),
    rdp_chunk_bridge: clipboard_files::ChunkBridge = clipboard_files::ChunkBridge::new(),
    rdp_resize_requested_at: Option<Instant> = None,
    rdp_resize_pending: Option<(u16, u16)> = None,
    rdp_resize_pending_since: Option<Instant> = None,
    display_scale: f32 = 1.0,
    vnc_pressed_keys: Vec<(keyboard::key::Physical, u32)> = Vec::new(),
    rdp_pressed_keys: HashSet<(u8, bool)> = HashSet::new(),
    vnc_fit: bool = false,
    vnc_grab_keys: bool = false,
    vnc_preedit: Option<iced::advanced::input_method::Preedit> = None,
    native_preedit: Option<iced::advanced::input_method::Preedit> = None,
    vnc_input_epoch: u64 = 1,
    desktop_window_focused: bool = false,
    vnc_cursor: Option<adit_session::VncCursor> = None,
    vnc_cursor_handle: Option<iced::widget::image::Handle> = None,
    modifiers: keyboard::Modifiers = keyboard::Modifiers::default(),
    window_width: f32 = 960.0,
    window_height: f32 = 640.0,
    sidebar_visible: bool = false,
    sidebar_width: f32 = 260.0,
    fullscreen: bool = false,
    startup_maximize_pending: bool = false,
    sidebar_dragging: bool = false,
    cursor_pos: Point = Point::ORIGIN,
    context_menu_pos: Point = Point::ORIGIN,
    main_view: MainView = MainView::Terminal,
    missions: Missions = Missions::default(),
    desktop_panes: desktop_panes::DesktopPanes = Default::default(),
    pending_paste: Option<String> = None,
    pending_paste_session: Option<SessionId> = None,
    paste_confirm_open: bool = false,
    mouse_button_down: bool = false,
    mouse_report_cell: Option<TerminalPoint> = None,
    search_open: bool = false,
    search_query: String = String::new(),
    search_matches: Vec<SearchMatch> = Vec::new(),
    search_index: Option<usize> = None,
    broadcast_input: bool = false,
    command_window_open: bool = false,
    command_target: CommandTarget = CommandTarget::ActiveSession,
    command_send_immediately: bool = false,
    command_history: Vec<String> = Vec::new(),
    command_history_pos: Option<usize> = None,
    panes: Vec<SessionId> = Vec::new(),
    focused_pane: usize = 0,
    tile_mode: TileMode = TileMode::Grid,
    rdp_toolbar_collapsed: bool = true,
    rdp_toolbar_hovered: bool = false,
    rdp_quality_menu_open: bool = false,
    last_error: Option<String> = None,
    notice: String = String::new(),
}

macro_rules! clipboard_state {
    ($($field:ident: $ty:ty = $initial:expr),* $(,)?) => {
        pub(crate) struct ClipboardLease { $($field: $ty,)* }
        impl Default for ClipboardLease {
            fn default() -> Self { Self { $($field: $initial,)* } }
        }
        impl ClipboardLease {
            pub fn swap_app(&mut self, app: &mut AditApp) {
                $(std::mem::swap(&mut self.$field, &mut app.$field);)*
            }
            pub fn swap_window(&mut self, window: &mut WindowPresentation) {
                $(std::mem::swap(&mut self.$field, &mut window.$field);)*
            }
        }
    }
}

clipboard_state! {
    rdp_clipboard_offered: Option<String> = None,
    rdp_clipboard_seq: u32 = 0,
    rdp_image_offered_seq: u32 = 0,
    rdp_clipboard_ticks: u8 = 0,
    desktop_clipboard_epoch: u64 = 0,
    rdp_offered_files: Vec<clipboard_files::OfferedFile> = Vec::new(),
    rdp_chunk_bridge: clipboard_files::ChunkBridge = clipboard_files::ChunkBridge::new(),
}

impl ClipboardLease {
    #[cfg(test)]
    pub fn test_bridge(&self) -> clipboard_files::ChunkBridge {
        self.rdp_chunk_bridge.clone()
    }

    pub fn revoke_files(&mut self) {
        self.rdp_offered_files.clear();
        self.rdp_chunk_bridge.close();
    }

    pub fn service_streams(&mut self, manager: &mut SessionManager, enabled: bool) {
        for chunk in manager.take_rdp_file_chunks() {
            if enabled {
                self.rdp_chunk_bridge.deliver(chunk.stream_id, chunk.data);
            }
        }
        for read in manager.take_rdp_file_reads() {
            let data = clipboard_files::read_offered_range(
                enabled,
                &self.rdp_offered_files,
                read.index,
                read.offset,
                read.length,
            );
            manager.answer_rdp_file_read(read.stream_id, data);
        }
    }
}

impl Drop for ClipboardLease {
    fn drop(&mut self) {
        self.rdp_chunk_bridge.close();
    }
}

impl WindowPresentation {
    pub fn adopt_terminal_position(&mut self, app: &mut AditApp) {
        // Before resizing into primary's geometry, selection anchors must be
        // interpreted against the grid that was actually visible in this window.
        std::mem::swap(&mut self.terminal_size, &mut app.terminal_size);
        std::mem::swap(&mut self.terminal_selection, &mut app.terminal_selection);
        std::mem::swap(
            &mut self.terminal_scroll_offset,
            &mut app.terminal_scroll_offset,
        );
        std::mem::swap(&mut self.vnc_fit, &mut app.vnc_fit);
    }
}

pub(crate) struct WindowScene {
    pub snapshot: TerminalSnapshot,
    pub style: TerminalStyle,
    pub highlights: Vec<Vec<(usize, usize, bool)>>,
    pub links_clickable: bool,
    pub desktop: bool,
    pub vnc: bool,
    pub scale: (f32, f32),
    pub previous_scale: (f32, f32),
    pub surface: (u16, u16),
    pub region: (f32, f32),
    pub cursor: Option<desktop_panes::RdpCursorVisual>,
    pub input: bool,
    pub read_only: bool,
}

impl WindowScene {
    pub fn capture(app: &AditApp, session: SessionId) -> Self {
        let snapshot = active_terminal_snapshot(app);
        let surface = app.rdp_surface_size.unwrap_or((0, 0));
        let scale = app.display_scale.max(0.1);
        let (fx, fy) = rdp_fit_factors(app, surface);
        let previous = app.rdp_surface_size_prev.unwrap_or((0, 0));
        let (px, py) = rdp_fill_factors(app, previous);
        Self {
            highlights: search_highlights_for(app, &snapshot),
            snapshot,
            style: terminal_style_for(app, Some(session)),
            links_clickable: link_open_modifier(app),
            desktop: app.manager.session_is_desktop(session),
            vnc: app.manager.active_is_vnc(),
            scale: (scale / fx, scale / fy),
            previous_scale: (scale / px, scale / py),
            surface,
            region: terminal_region_area(
                app.window_width,
                app.window_height,
                0.0,
                chrome_height(app),
            ),
            cursor: desktop_panes::cursor_visual(app, session),
            input: remote_workspace_accepts_input(app),
            read_only: app.manager.session_desktop_read_only(session),
        }
    }
}

pub(crate) fn surface_view(
    app: &AditApp,
    session: SessionId,
    state: &WindowPresentation,
    scene: &WindowScene,
) -> Element<'static, Message> {
    if !scene.desktop {
        let grid = terminal_view(
            Some(session),
            scene.style,
            scene.snapshot.clone(),
            state.terminal_focused,
            state.terminal_selection,
            state.terminal_scroll_offset,
            scene.highlights.clone(),
            scene.links_clickable,
            state.terminal_focused && state.cursor_blink_on,
            true,
        );
        return stack![
            mouse_area(grid)
                .on_press(Message::BeginTerminalSelection)
                .on_release(Message::EndTerminalSelection)
                .on_right_press(Message::ShowTerminalContextMenu)
                .on_move(Message::TerminalPointerMoved)
                .on_scroll(Message::TerminalScrolled)
                .interaction(mouse::Interaction::Text),
            native_ime::layer(
                session,
                state.vnc_input_epoch,
                scene.input,
                state.native_preedit.clone()
            ),
        ]
        .into();
    }
    let error = app
        .manager
        .session_desktop_error(session)
        .map(str::to_owned);
    let status = app.manager.session_summary(session).map(|s| s.status);
    let (sw, sh) = scene.surface;
    if error.is_some()
        || state.rdp_tiles.is_empty()
        || sw == 0
        || sh == 0
        || matches!(
            status,
            Some(SessionStatus::Error | SessionStatus::Disconnected) | None
        )
    {
        return container(
            text(error.unwrap_or_else(|| match status {
                Some(SessionStatus::Connecting | SessionStatus::Connected) => {
                    "等待远程桌面画面…".into()
                }
                _ => "连接已断开，按 Enter 重新连接".into(),
            }))
            .size(13),
        )
        .center_x(Fill)
        .center_y(Fill)
        .into();
    }
    let (sx, sy) = scene.scale;
    let logical = iced::Size::new(f32::from(sw) / sx, f32::from(sh) / sy);
    let mut layers = Vec::new();
    if !state.rdp_tiles_prev.is_empty() {
        layers.push(rdp_tile_grid(
            &state.rdp_tiles_prev,
            scene.previous_scale.0,
            scene.previous_scale.1,
        ));
    }
    layers.push(rdp_tile_grid(&state.rdp_tiles, sx, sy));
    if scene.vnc {
        layers.push(Element::new(vnc_cursor::CursorLayer {
            cursor: state.vnc_cursor.clone(),
            handle: state.vnc_cursor_handle.clone(),
            logical_size: logical,
            scale_x: sx,
            scale_y: sy,
            session_id: session,
            input_epoch: state.vnc_input_epoch,
            ime_enabled: scene.input && !scene.read_only,
            preedit: state.vnc_preedit.clone(),
        }));
    } else {
        layers.push(desktop_panes::cursor_visual_layer(
            scene.cursor.clone(),
            session,
            sx,
            sy,
            logical,
        ));
        layers.push(native_ime::surface_layer(
            session,
            state.vnc_input_epoch,
            scene.input && !scene.read_only,
            state.native_preedit.clone(),
            logical,
        ));
    }
    let surface = mouse_area(stack(layers))
        .on_move(move |p| {
            Message::RdpPointerMoved(Point::new(
                (p.x * sx).clamp(0.0, f32::from(sw) - 1.0),
                (p.y * sy).clamp(0.0, f32::from(sh) - 1.0),
            ))
        })
        .on_press(Message::RdpPressed(mouse::Button::Left))
        .on_release(Message::RdpReleased(mouse::Button::Left))
        .on_right_press(Message::RdpPressed(mouse::Button::Right))
        .on_right_release(Message::RdpReleased(mouse::Button::Right))
        .on_middle_press(Message::RdpPressed(mouse::Button::Middle))
        .on_middle_release(Message::RdpReleased(mouse::Button::Middle))
        .on_scroll(Message::RdpScrolled);
    let body: Element<'static, Message> =
        if logical.width > scene.region.0 + 1.0 || logical.height > scene.region.1 + 1.0 {
            scrollable(surface)
                .direction(iced::widget::scrollable::Direction::Both {
                    vertical: iced::widget::scrollable::Scrollbar::new()
                        .width(0)
                        .scroller_width(0),
                    horizontal: iced::widget::scrollable::Scrollbar::new()
                        .width(0)
                        .scroller_width(0),
                })
                .into()
        } else {
            surface.into()
        };
    container(body)
        .center_x(Fill)
        .center_y(Fill)
        .style(|_| container::Style {
            background: Some(Color::BLACK.into()),
            ..Default::default()
        })
        .into()
}
