//! Single-process native windows. DesktopApplication is the application-level
//! owner of AditApp's sole SessionManager and global services. Windows hold IDs,
//! presentation, and sampled pixels; detaching never spawns a protocol.
use super::*;
use iced::widget::column;
use std::collections::HashMap;
use window_presentation::{ClipboardLease, WindowPresentation, WindowScene};

#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
mod accessibility_acceptance;
#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub use accessibility_acceptance::run as run_accessibility_acceptance;

pub(crate) const HEADER_HEIGHT: f32 = 36.0;

#[derive(Debug, Clone)]
pub(crate) enum NativeMessage {
    Window(window::Id, Message),
    View(window::Id, Option<SessionId>, u64, Message),
    Runtime(window::Id, iced::Event, event::Status),
    Frame(window::Id),
    Tick,
    Blink,
    Selection,
    Opened(window::Id),
}

struct DetachedWindow {
    session: SessionId,
    state: WindowPresentation,
    scene: Option<WindowScene>,
}

pub(crate) struct DesktopApplication {
    app: AditApp,
    primary: window::Id,
    focused: Option<window::Id>,
    windows: HashMap<window::Id, DetachedWindow>,
    clipboard_leases: HashMap<SessionId, ClipboardLease>,
    modal_owner: Option<window::Id>,
}

pub(crate) fn run(
    boot: impl Fn() -> (AditApp, Task<Message>) + 'static,
    mut settings: window::Settings,
) -> iced::Result {
    settings.exit_on_close_request = false;
    iced::daemon(
        move || {
            let (mut app, startup) = boot();
            let (primary, opened) = window::open(settings.clone());
            app.native_main_window = Some(primary);
            app.native_window_id = Some(primary);
            let desktop = DesktopApplication {
                app,
                primary,
                focused: None,
                windows: HashMap::new(),
                clipboard_leases: HashMap::new(),
                modal_owner: None,
            };
            (
                desktop,
                Task::batch([
                    opened.map(NativeMessage::Opened),
                    startup.map(move |message| NativeMessage::Window(primary, message)),
                ]),
            )
        },
        DesktopApplication::update,
        DesktopApplication::view,
    )
    .title(|app: &DesktopApplication, id| app.title(id))
    .theme(|app: &DesktopApplication, _id| app_theme(&app.app))
    .subscription(DesktopApplication::subscription)
    .run()
}

pub(crate) fn prompt_visible(app: &AditApp, session: SessionId) -> bool {
    app.native_window_id.is_none_or(|window| {
        app.native_session_windows
            .get(&session)
            .copied()
            .or(app.native_main_window)
            == Some(window)
    })
}

pub(crate) fn sftp_visible(app: &AditApp) -> bool {
    app.native_window_id
        .is_none_or(|window| app.native_sftp_window.or(app.native_main_window) == Some(window))
}

pub(crate) fn dialog_visible(app: &AditApp) -> bool {
    app.native_window_id
        .is_none_or(|window| app.native_dialog_window.or(app.native_main_window) == Some(window))
}

pub(crate) fn owns_selected_session(app: &AditApp) -> bool {
    app.native_window_id.is_none_or(|window| {
        app.manager.active_session().is_some_and(|session| {
            app.native_session_windows
                .get(&session)
                .copied()
                .or(app.native_main_window)
                == Some(window)
        })
    })
}

fn input_message(message: &Message) -> bool {
    matches!(
        message,
        Message::KeyboardInput(_)
            | Message::DialogFocusActivate { .. }
            | Message::TerminalInputChanged(_)
            | Message::CommandHistoryPrev
            | Message::CommandHistoryNext
            | Message::ModifiersChanged(_)
            | Message::NativeImeEvent(..)
            | Message::VncImeEvent(..)
            | Message::ClipboardPasted(..)
            | Message::RdpClipboardPolled(..)
            | Message::PasteIntoTerminal
            | Message::ConfirmPaste
            | Message::SendTerminalInput
            | Message::RdpPointerMoved(_)
            | Message::RdpPressed(_)
            | Message::RdpReleased(_)
            | Message::RdpScrolled(_)
            | Message::TerminalPointerMoved(_)
            | Message::TerminalScrolled(_)
            | Message::PaneScrolled(..)
            | Message::BeginTerminalSelection
            | Message::EndTerminalSelection
            | Message::GlobalPointerReleased(..)
            | Message::DesktopPaneMoved(..)
            | Message::DesktopPanePressed(..)
            | Message::DesktopPaneReleased(..)
            | Message::DesktopPaneScrolled(..)
            | Message::ToggleFullscreen
            | Message::ToggleVncGrab
            | Message::ToggleVncFit
            | Message::ToggleVncReadOnly
            | Message::ToggleVncResize
            | Message::RdpSendCtrlAltDel
    )
}

impl DesktopApplication {
    fn known(&self, id: window::Id) -> bool {
        id == self.primary || self.windows.contains_key(&id)
    }
    fn owner(&self, session: SessionId) -> window::Id {
        self.app
            .native_session_windows
            .get(&session)
            .copied()
            .unwrap_or(self.primary)
    }
    fn title(&self, id: window::Id) -> String {
        if id != self.primary
            && self
                .windows
                .get(&id)
                .is_some_and(|window| self.app.manager.session_summary(window.session).is_none())
        {
            return "会话已关闭 — Adit".into();
        }
        self.windows
            .get(&id)
            .and_then(|w| self.app.manager.session_summary(w.session))
            .map(|s| format!("{} — Adit", s.title))
            .unwrap_or_else(|| app_title(&self.app))
    }

    /// Project only presentation and the active-session selector. The actual
    /// manager and every connection stay at the same address in self.app.
    fn in_window(
        &mut self,
        id: window::Id,
        action: impl FnOnce(&mut AditApp) -> Task<Message>,
    ) -> Task<NativeMessage> {
        let had_sftp = self.app.manager.sftp_is_open();
        let had_dialog = self.app.connection_dialog.is_some();
        let before_session = self.app.manager.active_session();
        if id == self.primary {
            self.app.native_window_id = Some(id);
            self.app.native_auxiliary = false;
            let task = action(&mut self.app);
            if before_session != self.app.manager.active_session() {
                self.park_clipboard(before_session);
                self.restore_clipboard(self.app.manager.active_session());
            }
            if !had_sftp && self.app.manager.sftp_is_open() {
                self.app.native_sftp_window = Some(id);
            }
            if !had_dialog && self.app.connection_dialog.is_some() {
                self.app.native_dialog_window = Some(id);
            }
            return task.map(move |m| NativeMessage::Window(id, m));
        }
        let Some(mut window) = self.windows.remove(&id) else {
            return Task::none();
        };
        // The user can explicitly close a connection from the primary session
        // list. Never let a stale attached window borrow primary's active host.
        if self.app.manager.session_summary(window.session).is_none() {
            window.scene = None;
            self.windows.insert(id, window);
            return Task::none();
        }
        let main_session = self.app.manager.active_session();
        window.state.swap(&mut self.app);
        self.app.native_window_id = Some(id);
        self.app.native_auxiliary = true;
        let _ = self.app.manager.select_session_context(window.session);
        let task = action(&mut self.app);
        if !had_sftp && self.app.manager.sftp_is_open() {
            self.app.native_sftp_window = Some(id);
        }
        if !had_dialog && self.app.connection_dialog.is_some() {
            self.app.native_dialog_window = Some(id);
        }
        // Alt+P may open an SFTP shell using the same connection. Move the
        // window to that new session, leaving the former one owned by main.
        if let Some(active) = self.app.manager.active_session() {
            if active != window.session && !self.app.native_session_windows.contains_key(&active) {
                self.park_clipboard(Some(window.session));
                self.restore_clipboard(Some(active));
                self.app.native_session_windows.remove(&window.session);
                window.session = active;
                self.app.native_session_windows.insert(active, id);
            }
        }
        window.scene = Some(WindowScene::capture(&self.app, window.session));
        window.state.swap(&mut self.app);
        self.app.native_window_id = Some(self.primary);
        self.app.native_auxiliary = false;
        if let Some(main) = main_session {
            let _ = self.app.manager.select_session_context(main);
        }
        self.windows.insert(id, window);
        task.map(move |m| NativeMessage::Window(id, m))
    }

    fn release(&mut self, id: window::Id) {
        let _ = self.in_window(id, |app| {
            release_rdp_modifiers(app);
            if let Some(session) = app.manager.active_session() {
                app.manager.discard_clipboard_offers(session);
            }
            app.desktop_window_focused = false;
            app.modifiers = keyboard::Modifiers::default();
            Task::none()
        });
    }

    fn focus(&mut self, id: window::Id) {
        if self.focused == Some(id) {
            return;
        }
        if let Some(old) = self.focused.take() {
            self.release(old);
        }
        self.focused = Some(id);
        let _ = self.in_window(id, |app| {
            app.desktop_window_focused = true;
            // A focus transfer starts a fresh composition lease.
            app.vnc_input_epoch = app.vnc_input_epoch.wrapping_add(1);
            Task::none()
        });
    }

    fn detach(&mut self, session: SessionId) -> Task<NativeMessage> {
        if let Some(id) = self.app.native_session_windows.get(&session) {
            return window::gain_focus(*id);
        }
        if self.app.manager.session_summary(session).is_none() {
            return Task::none();
        }
        let was_active = self.app.manager.active_session() == Some(session);
        if was_active {
            release_rdp_modifiers(&mut self.app);
            self.park_clipboard(Some(session));
        }
        let (id, task) = window::open(window::Settings {
            size: iced::Size::new(960.0, 640.0),
            icon: app_icon(),
            exit_on_close_request: false,
            ..Default::default()
        });
        self.app.native_session_windows.insert(session, id);
        let mut state = WindowPresentation::default();
        if was_active {
            state.terminal_size = self.app.terminal_size;
            state.terminal_selection = self.app.terminal_selection.take();
            state.terminal_scroll_offset = std::mem::take(&mut self.app.terminal_scroll_offset);
            state.vnc_fit = self.app.vnc_fit;
        }
        if let Some(mut lease) = self.clipboard_leases.remove(&session) {
            lease.swap_window(&mut state);
        }
        self.windows.insert(
            id,
            DetachedWindow {
                session,
                state,
                scene: None,
            },
        );
        self.app.panes.retain(|s| *s != session);
        if self.app.main_view == MainView::Missions {
            restore_mission(&mut self.app);
        } else if self.app.manager.active_session() == Some(session) {
            let next = self
                .app
                .manager
                .sessions()
                .iter()
                .find(|s| self.owner(s.id) == self.primary)
                .map(|s| s.id);
            if let Some(next) = next {
                let _ = self.app.manager.activate(next);
            } else {
                self.app.main_view = MainView::Hosts;
                self.app.terminal_focused = false;
            }
        }
        if was_active {
            self.restore_clipboard(self.app.manager.active_session());
        }
        sync_terminal_size(&mut self.app);
        let _ = self.in_window(id, |app| {
            sync_terminal_size(app);
            update(app, Message::RdpTick)
        });
        task.map(NativeMessage::Opened)
    }

    fn merge(&mut self, id: window::Id, close_native: bool) -> Task<NativeMessage> {
        if id == self.primary {
            return Task::none();
        }
        self.release(id);
        let Some(mut window) = self.windows.remove(&id) else {
            return Task::none();
        };
        self.app.accessibility.closed(id);
        self.app.native_session_windows.remove(&window.session);
        if self.focused == Some(id) {
            self.focused = None;
        }
        if self.app.native_sftp_window == Some(id) {
            self.app.native_sftp_window = Some(self.primary);
        }
        if self.app.native_dialog_window == Some(id) {
            self.app.native_dialog_window = Some(self.primary);
        }
        if self.app.manager.session_summary(window.session).is_none() {
            window.state.rdp_chunk_bridge.close();
            return if close_native {
                window::close(id)
            } else {
                Task::none()
            };
        }
        if self.app.manager.active_session() != Some(window.session) {
            release_rdp_modifiers(&mut self.app);
        }
        self.park_clipboard(self.app.manager.active_session());
        let mut lease = ClipboardLease::default();
        lease.swap_window(&mut window.state);
        self.clipboard_leases.insert(window.session, lease);
        let returns_to_mission = self.app.main_view == MainView::Missions
            && mission_contains_member(&self.app, window.session);
        let _ = self.app.manager.activate(window.session);
        window.state.adopt_terminal_position(&mut self.app);
        if returns_to_mission {
            activate_mission_session(&mut self.app, window.session);
        } else {
            self.app.main_view = MainView::Terminal;
            self.app.panes.clear();
            self.app.focused_pane = 0;
            self.app.terminal_focused = true;
        }
        self.restore_clipboard(self.app.manager.active_session());
        sync_terminal_size(&mut self.app);
        self.app.notice = "会话已合回主窗口，连接保持".into();
        if close_native {
            Task::batch([window::close(id), window::gain_focus(self.primary)])
        } else {
            Task::none()
        }
    }

    fn dispatch(&mut self, id: window::Id, message: Message) -> Task<NativeMessage> {
        if !self.known(id) {
            accessibility::cancel_discarded_message(&message);
            return Task::none();
        } // drop late commits/tasks from a closed window
        if input_message(&message) && self.focused != Some(id) {
            accessibility::cancel_discarded_message(&message);
            return Task::none();
        }
        if input_message(&message) && self.modal_owner.is_some_and(|owner| owner != id) {
            accessibility::cancel_discarded_message(&message);
            return Task::none();
        }
        let routed_message = match &message {
            Message::DialogFocusActivate { action, .. } => action.as_ref(),
            _ => &message,
        };
        // Global SFTP/credential services can outlive an explicitly closed tab.
        // Move their modal back to primary instead of borrowing its live host
        // as the stale window's input target or trapping an unclosable modal.
        if id != self.primary
            && self
                .windows
                .get(&id)
                .is_some_and(|window| self.app.manager.session_summary(window.session).is_none())
            && (is_sftp(&message)
                || matches!(
                    routed_message,
                    Message::AuthPromptInput { .. }
                        | Message::SubmitAuthPrompt { .. }
                        | Message::CancelAuthPrompt { .. }
                        | Message::RespondHostKey { .. }
                        | Message::ConfirmConnection
                        | Message::CancelConnection
                        | Message::ConnectionPasswordChanged(_)
                        | Message::RememberConnectionPasswordChanged(_)
                ))
        {
            if let Some(window) = self.windows.get(&id) {
                self.app.native_session_windows.remove(&window.session);
            }
            if self.app.native_sftp_window == Some(id) {
                self.app.native_sftp_window = Some(self.primary);
            }
            if self.app.native_dialog_window == Some(id) {
                self.app.native_dialog_window = Some(self.primary);
            }
            let task = self.dispatch(self.primary, message);
            return Task::batch([task, window::gain_focus(self.primary)]);
        }
        match routed_message {
            Message::DetachSession(session) => return self.detach(*session),
            Message::MergeNativeWindow => return self.merge(id, true),
            Message::TabClicked(session) | Message::ShowTabContextMenu(session) => {
                let owner = self.owner(*session);
                if owner != id {
                    return window::gain_focus(owner);
                }
            }
            Message::RespondHostKey { session_id, .. }
            | Message::SubmitAuthPrompt { session_id, .. }
            | Message::CancelAuthPrompt { session_id, .. } => {
                if self.owner(*session_id) != id {
                    accessibility::cancel_discarded_message(&message);
                    return Task::none();
                }
            }
            Message::AuthPromptInput { .. } => {
                if self
                    .app
                    .auth_prompt
                    .as_ref()
                    .is_none_or(|(sid, _)| self.owner(*sid) != id)
                {
                    return Task::none();
                }
            }
            Message::ConfirmConnection
            | Message::CancelConnection
            | Message::ConnectionPasswordChanged(_)
            | Message::RememberConnectionPasswordChanged(_)
                if self.app.native_dialog_window.unwrap_or(self.primary) != id =>
            {
                return Task::none();
            }
            _ => {}
        }
        if is_sftp(&message)
            && self.app.manager.sftp_is_open()
            && self.app.native_sftp_window.unwrap_or(self.primary) != id
        {
            return window::gain_focus(self.app.native_sftp_window.unwrap_or(self.primary));
        }
        if matches!(message, Message::RunMenu(MenuCommand::Sftp))
            && self.app.manager.sftp_is_open()
            && self.app.native_sftp_window.unwrap_or(self.primary) != id
        {
            return window::gain_focus(self.app.native_sftp_window.unwrap_or(self.primary));
        }
        let old_style = (
            self.app.font_family.clone(),
            self.app.font_size,
            self.app.color_scheme.clone(),
        );
        let had_sftp = self.app.manager.sftp_is_open();
        let had_dialog = self.app.connection_dialog.is_some();
        let revoke_files = matches!(message, Message::ToggleRdpClipboard(false));
        let task = self.in_window(id, |app| update(app, message));
        if revoke_files && !self.app.rdp_clipboard {
            self.app.rdp_offered_files.clear();
            self.app.rdp_chunk_bridge.close();
            for window in self.windows.values_mut() {
                window.state.rdp_offered_files.clear();
                window.state.rdp_chunk_bridge.close();
            }
            for lease in self.clipboard_leases.values_mut() {
                lease.revoke_files();
            }
        }
        if !had_sftp && self.app.manager.sftp_is_open() {
            self.app.native_sftp_window = Some(id);
        }
        if !self.app.manager.sftp_is_open() {
            self.app.native_sftp_window = None;
        }
        if !had_dialog && self.app.connection_dialog.is_some() {
            self.app.native_dialog_window = Some(id);
        }
        if self.app.connection_dialog.is_none() {
            self.app.native_dialog_window = None;
        }
        if old_style
            != (
                self.app.font_family.clone(),
                self.app.font_size,
                self.app.color_scheme.clone(),
            )
        {
            self.sync_presentations();
        }
        task
    }

    fn park_clipboard(&mut self, session: Option<SessionId>) {
        if let Some(session) = session {
            let mut lease = ClipboardLease::default();
            lease.swap_app(&mut self.app);
            self.clipboard_leases.insert(session, lease);
        }
    }

    fn restore_clipboard(&mut self, session: Option<SessionId>) {
        if let Some(mut lease) = session.and_then(|id| self.clipboard_leases.remove(&id)) {
            lease.swap_app(&mut self.app);
        }
    }

    fn service_parked_clipboards(&mut self) {
        let previous = self.app.manager.active_session();
        self.clipboard_leases
            .retain(|id, _| self.app.manager.session_summary(*id).is_some());
        for (session, lease) in &mut self.clipboard_leases {
            if self.app.manager.select_session_context(*session).is_ok() {
                lease.service_streams(&mut self.app.manager, self.app.rdp_clipboard);
            }
        }
        if let Some(previous) = previous {
            let _ = self.app.manager.select_session_context(previous);
        }
    }

    fn sync_presentations(&mut self) {
        sync_terminal_size(&mut self.app);
        let ids: Vec<_> = self.windows.keys().copied().collect();
        for id in ids {
            let _ = self.in_window(id, |app| {
                sync_terminal_size(app);
                Task::none()
            });
        }
    }

    fn sync_modal_owner(&mut self) {
        if let Some(session) = self
            .app
            .connection_dialog
            .as_ref()
            .and_then(|dialog| dialog.retry_session)
        {
            self.app.native_dialog_window = Some(self.owner(session));
        }
        let owner = self
            .app
            .manager
            .pending_host_key()
            .map(|(sid, _)| self.owner(sid))
            .or_else(|| {
                self.app
                    .auth_prompt
                    .as_ref()
                    .map(|(sid, _)| self.owner(*sid))
            })
            .or_else(|| {
                self.app
                    .connection_dialog
                    .as_ref()
                    .map(|_| self.app.native_dialog_window.unwrap_or(self.primary))
            })
            .or_else(|| {
                self.app
                    .manager
                    .sftp_is_open()
                    .then(|| self.app.native_sftp_window.unwrap_or(self.primary))
            });
        if owner != self.modal_owner {
            self.modal_owner = owner;
            if owner.is_some() {
                let ids: Vec<_> = std::iter::once(self.primary)
                    .chain(self.windows.keys().copied())
                    .collect();
                for id in ids {
                    if owner != Some(id) {
                        accessibility::block_background_window(id);
                    }
                    let focused = self.focused == Some(id);
                    self.release(id);
                    let _ = self.in_window(id, |app| {
                        app.desktop_window_focused = focused;
                        Task::none()
                    });
                }
            }
        }
    }

    fn update(&mut self, message: NativeMessage) -> Task<NativeMessage> {
        let task = match message {
            NativeMessage::Window(id, message) => self.dispatch(id, message),
            NativeMessage::View(id, session, epoch, message) => {
                let (same_session, same_epoch) = if id == self.primary {
                    (
                        self.app.manager.active_session() == session,
                        self.app.vnc_input_epoch == epoch,
                    )
                } else {
                    self.windows.get(&id).map_or((false, false), |window| {
                        (
                            Some(window.session) == session,
                            window.state.vnc_input_epoch == epoch,
                        )
                    })
                };
                // Confirmation activations have their own dialog revision and
                // lease. VNC input revocation during the same event batch must
                // not discard them, while session and foreground checks remain.
                let needs_input_epoch = !matches!(&message, Message::DialogFocusActivate { .. });
                if input_message(&message) && (!same_session || (needs_input_epoch && !same_epoch))
                {
                    accessibility::cancel_discarded_message(&message);
                    return Task::none();
                }
                self.dispatch(id, message)
            }
            NativeMessage::Opened(id) => self.in_window(id, |app| {
                sync_terminal_size(app);
                window::scale_factor(id).map(Message::DisplayScale)
            }),
            NativeMessage::Tick => {
                // Poll the sole manager in primary, then service each window's
                // presentation and clipboard streams without re-polling transports.
                let focused = self.focused.unwrap_or(self.primary);
                let _ = self.in_window(focused, |app| {
                    app.manager.poll_events();
                    Task::none()
                });
                let mut tasks = vec![self.dispatch(self.primary, Message::Tick)];
                let ids: Vec<_> = self.windows.keys().copied().collect();
                for id in ids {
                    tasks.push(self.dispatch(id, Message::Tick));
                }
                // A profile edit or cloud merge may change another window's
                // inherited metrics without resizing that native window.
                self.sync_presentations();
                self.service_parked_clipboards();
                persist_settings_if_changed(&mut self.app);
                Task::batch(tasks)
            }
            NativeMessage::Frame(id) => self.dispatch(id, Message::RdpTick),
            NativeMessage::Blink => {
                let ids: Vec<_> = std::iter::once(self.primary)
                    .chain(self.windows.keys().copied())
                    .collect();
                Task::batch(
                    ids.into_iter()
                        .map(|id| self.dispatch(id, Message::CursorBlink))
                        .collect::<Vec<_>>(),
                )
            }
            NativeMessage::Selection => self
                .focused
                .map(|id| self.dispatch(id, Message::SelectionAutoScroll))
                .unwrap_or_else(Task::none),
            NativeMessage::Runtime(id, event, status) => {
                if !self.known(id) {
                    return Task::none();
                }
                match &event {
                    iced::Event::Window(window::Event::CloseRequested) => {
                        if id == self.primary {
                            let ids: Vec<_> =
                                self.windows.keys().copied().chain([self.primary]).collect();
                            for id in ids {
                                self.release(id);
                            }
                            return iced::exit();
                        }
                        return self.merge(id, true);
                    }
                    iced::Event::Window(window::Event::Closed) => return self.merge(id, false),
                    iced::Event::Window(window::Event::Focused) => self.focus(id),
                    iced::Event::Window(window::Event::Unfocused) => {
                        // Unfocused(A) may arrive after Focused(B). Never let A
                        // disarm B's newly acquired process-wide system-key hook.
                        if self.focused != Some(id) {
                            return Task::none();
                        }
                        self.focused = None;
                        self.release(id);
                        return Task::none();
                    }
                    _ => {}
                }
                if matches!(event, iced::Event::Window(window::Event::FileDropped(_)))
                    && self.app.manager.sftp_is_open()
                    && self.app.native_sftp_window.unwrap_or(self.primary) != id
                {
                    return window::gain_focus(self.app.native_sftp_window.unwrap_or(self.primary));
                }
                if self.modal_owner.is_some_and(|owner| owner != id)
                    && matches!(
                        event,
                        iced::Event::Keyboard(_) | iced::Event::InputMethod(_)
                    )
                {
                    return Task::none();
                }
                if self.focused != Some(id)
                    && matches!(
                        event,
                        iced::Event::Keyboard(_)
                            | iced::Event::Mouse(_)
                            | iced::Event::InputMethod(_)
                    )
                {
                    return Task::none();
                }
                self.in_window(id, |app| {
                    let mut messages = Vec::new();
                    let runtime = if mission_name_editing(app) {
                        mission_name_runtime_event(event.clone(), status, id)
                    } else {
                        runtime_event(event.clone(), status, id)
                    };
                    messages.extend(runtime);
                    if app.terminal_selecting {
                        messages.extend(terminal_selection_event(event.clone(), status, id));
                    }
                    if app.sidebar_dragging {
                        messages.extend(sidebar_drag_event(event.clone(), status, id));
                    }
                    if app.scrollbar_dragging {
                        messages.extend(scrollbar_drag_event(event.clone(), status, id));
                    }
                    if app.dragged_tab.is_some() {
                        messages.extend(tab_release_event(event, status, id));
                    }
                    Task::batch(
                        messages
                            .into_iter()
                            .map(|m| update(app, m))
                            .collect::<Vec<_>>(),
                    )
                })
            }
        };
        self.sync_modal_owner();
        task
    }

    fn subscription(&self) -> Subscription<NativeMessage> {
        let mut subs = vec![
            iced::time::every(Duration::from_millis(100)).map(|_| NativeMessage::Tick),
            event::listen_with(|event, status, id| Some(NativeMessage::Runtime(id, event, status))),
        ];
        if terminal_cursor_blinks(&self.app)
            || self.windows.values().any(|w| {
                w.scene.as_ref().is_some_and(|scene| !scene.desktop) && w.state.terminal_focused
            })
        {
            subs.push(iced::time::every(Duration::from_millis(530)).map(|_| NativeMessage::Blink));
        }
        let video = self.app.manager.active_rdp_live()
            || desktop_panes::needs_frames(&self.app)
            || self.windows.values().any(|w| {
                self.app.manager.session_is_desktop(w.session)
                    && self.app.manager.session_desktop_error(w.session).is_none()
                    && self
                        .app
                        .manager
                        .session_summary(w.session)
                        .is_some_and(|s| {
                            matches!(
                                s.status,
                                SessionStatus::Connecting | SessionStatus::Connected
                            )
                        })
            })
            || cards_are_moving(&self.app);
        if video {
            subs.push(event::listen_raw(|event, _, id| match event {
                iced::Event::Window(window::Event::RedrawRequested(_)) => {
                    Some(NativeMessage::Frame(id))
                }
                _ => None,
            }));
        }
        if self.app.terminal_selecting || self.windows.values().any(|w| w.state.terminal_selecting)
        {
            subs.push(
                iced::time::every(Duration::from_millis(60)).map(|_| NativeMessage::Selection),
            );
        }
        Subscription::batch(subs)
    }

    fn view(&self, id: window::Id) -> Element<'_, NativeMessage> {
        if id == self.primary {
            let session = self.app.manager.active_session();
            let epoch = self.app.vnc_input_epoch;
            return view(&self.app).map(move |m| NativeMessage::View(id, session, epoch, m));
        }
        let Some(window) = self.windows.get(&id) else {
            return Space::new().into();
        };
        let state = &window.state;
        let namespace = accessibility::detached_namespace(
            &self.app,
            id,
            state.pending_hyperlink.is_some(),
            state.hyperlink_prompt_generation,
            state.paste_confirm_open,
            state.terminal_context_menu,
        );
        let _building = accessibility::building(Some(id), &namespace);
        let title = self
            .app
            .manager
            .session_summary(window.session)
            .map(|s| s.title)
            .unwrap_or_else(|| "会话已关闭".into());
        let mut bar = row![
            text(title).size(12).width(Fill),
            native_action("合回主窗", Message::MergeNativeWindow),
            native_action("搜索", Message::NativeOpenSearch),
            native_action(
                if state.fullscreen {
                    "退出全屏"
                } else {
                    "全屏"
                },
                Message::ToggleFullscreen
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        if window.scene.as_ref().is_some_and(|s| s.vnc) {
            bar = bar
                .push(native_action(
                    if state.vnc_fit { "原始" } else { "适应" },
                    Message::ToggleVncFit,
                ))
                .push(native_action(
                    if state.vnc_grab_keys {
                        "释放系统键"
                    } else {
                        "系统键"
                    },
                    Message::ToggleVncGrab,
                ))
                .push(native_action(
                    if window.scene.as_ref().is_some_and(|s| s.read_only) {
                        "允许控制"
                    } else {
                        "只读"
                    },
                    Message::ToggleVncReadOnly,
                ));
        }
        if window
            .scene
            .as_ref()
            .is_some_and(|s| s.desktop && !s.read_only)
        {
            bar = bar.push(native_action("Ctrl+Alt+Del", Message::RdpSendCtrlAltDel));
        }
        if self
            .app
            .manager
            .session_summary(window.session)
            .and_then(|summary| self.app.manager.profile(summary.profile_id))
            .is_some_and(|profile| profile.protocol.is_ssh_based())
        {
            bar = bar.push(native_action("SFTP", Message::RunMenu(MenuCommand::Sftp)));
        }
        let mut body = column![container(bar)
            .padding([0, 8])
            .center_y(HEADER_HEIGHT)
            .width(Fill)
            .style(|_| sidebar_style()),]
        .width(Fill)
        .height(Fill);
        if state.search_open {
            body = body.push(
                row![
                    text_input(t("搜索终端输出"), &state.search_query)
                        .id(search_input_id())
                        .on_input(Message::SearchQueryChanged)
                        .on_submit(Message::SearchNext),
                    button(t("上一个")).on_press(Message::SearchPrev),
                    button(t("下一个")).on_press(Message::SearchNext),
                    button("×").on_press(Message::CloseSearch),
                ]
                .height(TOOLBAR_HEIGHT)
                .align_y(Alignment::Center),
            );
        }
        let surface = window
            .scene
            .as_ref()
            .map(|scene| window_presentation::surface_view(&self.app, window.session, state, scene))
            .unwrap_or_else(|| {
                container(text(t("会话已关闭，可合回主窗口")))
                    .center_x(Fill)
                    .center_y(Fill)
                    .into()
            });
        body = body.push(surface);
        if state.command_window_open {
            body = body.push(
                container(
                    text_input(t("向当前会话发送一行"), &state.terminal_input)
                        .on_input(Message::TerminalInputChanged)
                        .on_submit(Message::SendTerminalInput)
                        .padding(8),
                )
                .height(TOOLBAR_HEIGHT),
            );
        }
        let status = state.last_error.as_deref().unwrap_or(&state.notice);
        body = body.push(
            container(text(status.to_owned()).size(11))
                .padding([0, 8])
                .center_y(STATUS_BAR_HEIGHT)
                .width(Fill),
        );
        let base = container(body)
            .style(|_| app_background_style())
            .width(Fill)
            .height(Fill);
        let mut layers: Vec<Element<'_, Message>> = vec![base.into()];
        if state.terminal_context_menu {
            layers.push(opaque(floating_context_menu_at(
                state.window_width,
                state.window_height,
                state.context_menu_pos,
                terminal_context_menu(),
                Message::HideTerminalContextMenu,
            )));
        }
        if self.app.native_sftp_window == Some(id) && self.app.manager.sftp_is_open() {
            layers.push(opaque(sftp_panel_overlay(&self.app)));
            if let Some((pane, name, is_dir)) = self.app.sftp_context_menu.clone() {
                layers.push(opaque(floating_context_menu_at(
                    state.window_width,
                    state.window_height,
                    state.context_menu_pos,
                    sftp_context_menu_card(pane, name, is_dir),
                    Message::HideSftpContextMenu,
                )));
            }
        }
        if self.app.native_dialog_window == Some(id) && self.app.connection_dialog.is_some() {
            layers.push(opaque(connection_dialog_overlay(&self.app)));
        }
        if let Some((sid, prompt)) = self
            .app
            .manager
            .pending_host_key()
            .filter(|(sid, _)| self.owner(*sid) == id)
        {
            layers.push(opaque(host_key_dialog_overlay(sid, &prompt)));
        }
        if let Some((sid, prompt)) = self
            .app
            .auth_prompt
            .as_ref()
            .filter(|(sid, _)| self.owner(*sid) == id)
        {
            layers.push(opaque(auth_prompt_dialog_overlay(
                *sid,
                prompt,
                &self.app.auth_prompt_answers,
            )));
        }
        if let Some(url) = &state.pending_hyperlink {
            layers.push(opaque(hyperlink_confirm_overlay(
                url,
                state.hyperlink_prompt_generation,
            )));
        }
        if state.paste_confirm_open {
            layers.push(opaque(
                container(
                    column![
                        text(t("确认粘贴")).size(16),
                        text(state.pending_paste.clone().unwrap_or_default()).size(12),
                        row![
                            button(t("取消")).on_press(Message::CancelPaste),
                            button(t("粘贴")).on_press(Message::ConfirmPaste)
                        ]
                        .spacing(8),
                    ]
                    .spacing(12),
                )
                .padding(20)
                .center_x(Fill)
                .center_y(Fill),
            ));
        }
        if self.modal_owner.is_some_and(|owner| owner != id) {
            layers.push(opaque(
                container(text(t("请在另一窗口完成验证或文件操作")))
                    .padding(12)
                    .center_x(Fill)
                    .center_y(Fill)
                    .style(|_| dialog_scrim_style()),
            ));
        }
        if layers.len() > 1 {
            let top = layers.pop().unwrap();
            layers = layers
                .into_iter()
                .map(|layer| dialog_focus::background(layer, true))
                .collect();
            let escape = (namespace == "menu" && state.terminal_context_menu)
                .then_some(Message::DismissMenu(MenuEscapeTarget::Terminal));
            layers.push(accessibility::modal(top, escape));
        } else {
            let base = layers.pop().unwrap();
            layers.push(dialog_focus::background(base, false));
        }
        let view: Element<'_, Message> =
            accessibility::scope(Some(id), namespace, stack(layers).into());
        let session = Some(window.session);
        let epoch = state.vnc_input_epoch;
        view.map(move |m| NativeMessage::View(id, session, epoch, m))
    }
}

fn native_action(label: &str, message: Message) -> iced::widget::Button<'static, Message> {
    button(text(label.to_owned()).size(11))
        .padding([4, 8])
        .style(|_, status| secondary_button_style(status))
        .on_press(message)
}

fn is_sftp(message: &Message) -> bool {
    matches!(
        message,
        Message::CloseSftp
            | Message::SftpNavigate(_)
            | Message::SftpUp
            | Message::SftpRefresh
            | Message::SftpLocalNavigate(_)
            | Message::SftpLocalUp
            | Message::SftpLocalRefresh
            | Message::SftpUploadLocal(_)
            | Message::SftpDownload(_)
            | Message::SftpRowPress(..)
            | Message::SftpCursorMoved(_)
            | Message::ShowSftpContextMenu(..)
            | Message::HideSftpContextMenu
            | Message::SftpTransferSelected(_)
            | Message::SftpFileDropped(_)
            | Message::SftpLocalPathChanged(_)
            | Message::SftpLocalGo
            | Message::SftpRemotePathChanged(_)
            | Message::SftpRemoteGo
            | Message::SftpUploadPathChanged(_)
            | Message::SftpUpload
            | Message::SftpPickUpload
            | Message::SftpUploadPicked(_)
            | Message::SftpNewFolderChanged(_)
            | Message::SftpMkdir
            | Message::SftpBeginRename(..)
            | Message::SftpRenameToChanged(_)
            | Message::SftpConfirmRename
            | Message::SftpCancelRename
            | Message::SftpBeginDelete(..)
            | Message::SftpConfirmDelete
            | Message::SftpCancelDelete
            | Message::SftpConfirmOverwrite
            | Message::SftpCancelOverwrite
            | Message::SftpSort(..)
            | Message::SftpClearTransfers
            | Message::SftpCancelTransfer(_)
            | Message::SftpCancelAll
            | Message::SftpDragEnter(_)
            | Message::SftpDragMove(..)
    )
}

#[cfg(feature = "native-workbench-acceptance")]
pub fn run_acceptance() -> iced::Result {
    let config = std::env::var_os("ADIT_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .expect("set ADIT_CONFIG_DIR to a fresh fixture directory");
    let config = config
        .canonicalize()
        .expect("create the fixture config directory first");
    assert!(
        config.starts_with(std::env::temp_dir().canonicalize().unwrap()),
        "native acceptance must not use the installed application's configuration"
    );
    run(
        || {
            let mut profiles: Vec<_> = (0..2)
                .map(|index| {
                    let mut profile = ConnectionProfile::new(
                        tf("原生验收 · 终端 {}", &[&(index + 1)]),
                        "",
                        22,
                        "",
                    );
                    profile.protocol = Protocol::LocalShell;
                    profile.identity_file = std::env::var("ADIT_NATIVE_SHELL").unwrap_or_default();
                    profile.session_options.terminal_appearance.font_size =
                        Some(if index == 0 { 13 } else { 20 });
                    profile
                })
                .collect();
            let vnc = std::env::var_os("ADIT_NATIVE_WINDOWS_VNC").is_some();
            if vnc {
                let mut profile =
                    ConnectionProfile::new("原生验收 · 回环 VNC", "127.0.0.1", 15915, "");
                profile.protocol = Protocol::Vnc;
                profile.vnc.security = VncSecurity::AllowNone;
                profile.vnc.clipboard_send = false;
                profile.vnc.clipboard_receive = false;
                profile.vnc.resize_remote = false;
                profiles.push(profile);
            }
            let ids: Vec<_> = profiles.iter().map(|p| p.id).collect();
            let mut app = AditApp {
                manager: SessionManager::with_profiles(profiles),
                auto_check_updates: false,
                keyring_migrated: true,
                main_view: MainView::Terminal,
                terminal_focused: true,
                sidebar_visible: true,
                startup_maximize_pending: false,
                rdp_clipboard: false,
                ..AditApp::default()
            };
            app.sync.auto_sync = false;
            app.manager.set_rdp_clipboard(false);
            let a = app
                .manager
                .open_live_ssh_session(ids[0], String::new(), String::new())
                .unwrap();
            let b = app
                .manager
                .open_live_ssh_session(ids[1], String::new(), String::new())
                .unwrap();
            let mut tasks = vec![Task::done(Message::DetachSession(b))];
            if vnc {
                let desktop = app
                    .manager
                    .open_live_vnc_session(ids[2], String::new())
                    .unwrap();
                tasks.push(Task::done(Message::DetachSession(desktop)));
            }
            app.manager.activate(a).unwrap();
            sync_terminal_size(&mut app);
            (app, Task::batch(tasks))
        },
        window::Settings {
            size: iced::Size::new(1000.0, 700.0),
            icon: app_icon(),
            ..Default::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::advanced::input_method::Event as Ime;

    #[test]
    fn disabling_clipboard_clears_parked_and_detached_file_state() {
        let (mut desktop, ids) = fixture();
        let offered = |name: &str| clipboard_files::OfferedFile {
            path: std::path::PathBuf::from(name),
            meta: adit_session::ClipFile {
                name: name.into(),
                size: 1,
                is_dir: false,
            },
        };
        desktop.app.rdp_offered_files = vec![offered("parked.bin")];
        let _ = desktop.dispatch(desktop.primary, Message::TabClicked(ids[1]));
        desktop.app.rdp_offered_files = vec![offered("primary.bin")];
        let _ = desktop.detach(ids[0]);
        let child = desktop.owner(ids[0]);
        let _ = desktop.in_window(child, |app| {
            app.rdp_offered_files = vec![offered("child.bin")];
            Task::none()
        });
        let parked = desktop.clipboard_leases[&ids[2]].test_bridge();
        let detached = desktop.windows[&child].state.rdp_chunk_bridge.clone();
        let primary = desktop.app.rdp_chunk_bridge.clone();
        assert!(parked.begin().is_ok());
        assert!(detached.begin().is_ok());
        assert!(primary.begin().is_ok());
        let _ = desktop.dispatch(child, Message::ToggleRdpClipboard(false));
        assert!(desktop.app.rdp_offered_files.is_empty());
        assert!(desktop.windows[&child].state.rdp_offered_files.is_empty());
        assert!(parked.begin().is_err());
        assert!(detached.begin().is_err());
        assert!(primary.begin().is_err());
    }

    fn fixture() -> (DesktopApplication, [SessionId; 3]) {
        let mut profiles: Vec<_> = (0..3)
            .map(|n| {
                ConnectionProfile::new(format!("window-{n}"), "fixture.invalid", 22, "fixture")
            })
            .collect();
        profiles[0].session_options.terminal_appearance.font_size = Some(10);
        profiles[1].session_options.terminal_appearance.font_size = Some(24);
        let mut manager = SessionManager::with_profiles(profiles);
        let ids: Vec<_> = manager.profiles().iter().map(|p| p.id).collect();
        let sessions = std::array::from_fn(|i| manager.open_mock_session(ids[i]).unwrap());
        let primary = window::Id::unique();
        let mut app = AditApp {
            manager,
            main_view: MainView::Terminal,
            terminal_focused: true,
            font_family: "Cascadia Mono".into(),
            font_size: 13.0,
            native_main_window: Some(primary),
            native_window_id: Some(primary),
            ..AditApp::default()
        };
        app.manager.activate(sessions[2]).unwrap();
        sync_terminal_size(&mut app);
        (
            DesktopApplication {
                app,
                primary,
                focused: Some(primary),
                windows: HashMap::new(),
                clipboard_leases: HashMap::new(),
                modal_owner: None,
            },
            sessions,
        )
    }

    fn text(app: &DesktopApplication, session: SessionId) -> String {
        snapshot_to_text(
            &app.app
                .manager
                .snapshot_for(session, Viewport::visible(200)),
        )
    }

    #[test]
    fn another_windows_modal_rejects_queued_command_edits_and_sends() {
        let (mut desktop, ids) = fixture();
        let _ = desktop.detach(ids[0]);
        let child = desktop.owner(ids[0]);
        desktop.focus(desktop.primary);
        desktop.app.command_window_open = true;
        desktop.app.command_send_immediately = true;
        desktop.app.terminal_input = "original".into();
        let profile = desktop
            .app
            .manager
            .session_summary(ids[0])
            .unwrap()
            .profile_id;
        desktop.app.connection_dialog = Some(ConnectionDialog {
            profile_id: profile,
            retry_session: Some(ids[0]),
            title: "synthetic".into(),
            endpoint: "fixture.invalid".into(),
            auth_method: AuthMethod::Password,
            identity_file: String::new(),
        });
        desktop.app.native_dialog_window = Some(child);
        desktop.sync_modal_owner();
        assert_eq!(desktop.modal_owner, Some(child));
        let before = text(&desktop, ids[2]);
        let _ = desktop.dispatch(
            desktop.primary,
            Message::TerminalInputChanged("original LEAK".into()),
        );
        let _ = desktop.dispatch(desktop.primary, Message::SendTerminalInput);
        let _ = desktop.dispatch(desktop.primary, Message::CommandHistoryPrev);
        let epoch = desktop.app.vnc_input_epoch;
        let _ = desktop.dispatch(
            desktop.primary,
            Message::NativeImeEvent(
                ids[2],
                epoch,
                iced::advanced::input_method::Event::Commit("SYNTHETIC-IME".into()),
            ),
        );
        assert_eq!(desktop.app.terminal_input, "original");
        assert_eq!(text(&desktop, ids[2]), before);
        desktop.app.connection_dialog = None;
        desktop.sync_modal_owner();
        let _ = desktop.dispatch(
            desktop.primary,
            Message::TerminalInputChanged("original ok".into()),
        );
        assert_eq!(desktop.app.terminal_input, "original ok");
        assert!(text(&desktop, ids[2]).contains(" ok"));
    }

    #[test]
    fn detached_uia_scroll_step_uses_current_window_position_after_mouse_scroll() {
        let (mut desktop, ids) = fixture();
        desktop.app.manager.activate(ids[0]).unwrap();
        desktop
            .app
            .manager
            .send_input_to_active(
                (0..150)
                    .map(|n| format!("line {n}\r\n"))
                    .collect::<String>(),
            )
            .unwrap();
        let _ = desktop.detach(ids[0]);
        let child = desktop.owner(ids[0]);
        desktop.app.accessibility.opening(desktop.primary);
        desktop.app.accessibility.opening(child);
        let _ = desktop.in_window(child, |app| {
            accessibility::scroll(app, child, ids[0], 20);
            assert_eq!(app.terminal_scroll_offset, 20);
            // A regular wheel or drag updates presentation without touching the UIA cache.
            app.terminal_scroll_offset = 7;
            accessibility::scroll_lines(app, child, ids[0], 3);
            assert_eq!(app.terminal_scroll_offset, 10);
            Task::none()
        });
        assert_eq!(desktop.windows[&child].state.terminal_scroll_offset, 10);
    }

    #[test]
    fn accessibility_detached_link_generations_stay_with_their_window() {
        let (mut desktop, ids) = fixture();
        let _ = desktop.detach(ids[0]);
        let child = desktop.owner(ids[0]);
        let url = "https://fixture.invalid/same";
        let _ = desktop.dispatch(child, Message::OpenHyperlink(url.into()));
        let first = desktop.windows[&child].state.hyperlink_prompt_generation;
        let _ = desktop.dispatch(child, Message::OpenHyperlink(url.into()));
        let second = desktop.windows[&child].state.hyperlink_prompt_generation;
        assert_ne!(first, second);
        let _ = desktop.dispatch(child, Message::CancelOpenHyperlink(first));
        assert_eq!(
            desktop.windows[&child].state.pending_hyperlink.as_deref(),
            Some(url)
        );
        assert!(desktop.app.pending_hyperlink.is_none());
        let _ = desktop.dispatch(child, Message::CancelOpenHyperlink(second));
        assert!(desktop.windows[&child].state.pending_hyperlink.is_none());
    }

    #[test]
    fn uia_focus_restores_only_the_detached_windows_original_session() {
        let (mut desktop, ids) = fixture();
        let _ = desktop.detach(ids[0]);
        let child = desktop.owner(ids[0]);
        desktop.app.accessibility.opening(desktop.primary);
        desktop.app.accessibility.opening(child);
        desktop.app.terminal_focused = false;
        let _ = desktop.in_window(child, |app| {
            app.terminal_focused = true;
            app.desktop_window_focused = true;
            let _ = update(
                app,
                Message::OpenHyperlink("https://fixture.invalid".into()),
            );
            let generation = app.hyperlink_prompt_generation;
            let _ = update(
                app,
                Message::AccessibilityFocus {
                    window: child,
                    id: iced::advanced::widget::Id::from("synthetic-link"),
                    ticket: None,
                    lease: None,
                },
            );
            assert!(!app.terminal_focused);
            let _ = update(app, Message::CancelOpenHyperlink(generation));
            assert!(app.terminal_focused);
            assert_eq!(app.manager.active_session(), Some(ids[0]));
            Task::none()
        });
        assert!(desktop.windows[&child].state.terminal_focused);
        assert!(!desktop.app.terminal_focused);
        assert_ne!(desktop.app.manager.active_session(), Some(ids[0]));
    }

    #[test]
    fn merging_another_window_cannot_redirect_a_pending_multiline_paste() {
        let (mut desktop, ids) = fixture();
        let _ = desktop.detach(ids[0]);
        let child = desktop.owner(ids[0]);
        desktop.focus(desktop.primary);
        desktop.app.manager.activate(ids[2]).unwrap();
        desktop.app.confirm_multiline_paste = true;
        let _ = desktop.dispatch(
            desktop.primary,
            Message::ClipboardPasted(Some(ids[2]), Some("first\nfixture_marker".into())),
        );
        assert!(desktop.app.paste_confirm_open);
        desktop.focus(child);
        let _ = desktop.dispatch(child, Message::MergeNativeWindow);
        desktop.focus(desktop.primary);
        let _ = desktop.dispatch(desktop.primary, Message::ConfirmPaste);
        assert!(!text(&desktop, ids[0]).contains("fixture_marker"));
        assert!(!text(&desktop, ids[2]).contains("fixture_marker"));
        assert!(!desktop.app.paste_confirm_open);
    }

    #[test]
    fn confirmed_multiline_paste_still_reaches_its_original_session() {
        let (mut desktop, ids) = fixture();
        desktop.app.confirm_multiline_paste = true;
        let _ = desktop.dispatch(
            desktop.primary,
            Message::ClipboardPasted(Some(ids[2]), Some("first\nfixture_marker".into())),
        );
        assert!(desktop.app.paste_confirm_open);
        let _ = desktop.dispatch(desktop.primary, Message::ConfirmPaste);
        assert!(text(&desktop, ids[2]).contains("fixture_marker"));
        assert!(!text(&desktop, ids[0]).contains("fixture_marker"));
    }

    #[test]
    fn delayed_clipboard_read_cannot_paste_into_a_new_active_session() {
        let (mut desktop, ids) = fixture();
        desktop.app.manager.activate(ids[0]).unwrap();
        let _ = desktop.dispatch(
            desktop.primary,
            Message::ClipboardPasted(Some(ids[2]), Some("stale_marker".into())),
        );
        assert!(!text(&desktop, ids[0]).contains("stale_marker"));
        assert!(!desktop.app.paste_confirm_open);
    }

    #[test]
    fn detach_and_close_return_to_the_original_mission_without_copying_or_closing_connections() {
        let (mut desktop, ids) = fixture();
        create_mission(&mut desktop.app, ids.to_vec());
        let members = mission_members_for_test(&desktop.app);
        let manager_address = &desktop.app.manager as *const SessionManager;
        let _ = desktop.detach(ids[0]);
        let child = desktop.owner(ids[0]);
        assert_ne!(child, desktop.primary);
        assert_eq!(
            &desktop.app.manager as *const SessionManager,
            manager_address
        );
        assert_eq!(desktop.app.manager.sessions().len(), 3);
        assert!(!desktop.app.panes.contains(&ids[0]));
        assert_eq!(mission_members_for_test(&desktop.app), members);
        let _ = desktop.view(child);
        let _ = desktop.update(NativeMessage::Runtime(
            child,
            iced::Event::Window(window::Event::CloseRequested),
            event::Status::Ignored,
        ));
        assert!(desktop.windows.is_empty());
        assert_eq!(desktop.app.manager.sessions().len(), 3);
        assert_eq!(mission_members_for_test(&desktop.app), members);
        assert!(desktop.app.panes.contains(&ids[0]));
        assert_eq!(
            &desktop.app.manager as *const SessionManager,
            manager_address
        );
    }

    #[test]
    fn each_window_has_its_own_font_geometry_dpi_scroll_and_primary_state_is_restored() {
        let (mut desktop, ids) = fixture();
        let primary_size = (
            desktop.app.window_width,
            desktop.app.window_height,
            desktop.app.display_scale,
        );
        let primary_session = desktop.app.manager.active_session();
        let _ = desktop.detach(ids[0]);
        let a = desktop.owner(ids[0]);
        let _ = desktop.detach(ids[1]);
        let b = desktop.owner(ids[1]);
        let _ = desktop.dispatch(
            a,
            Message::WindowResized {
                width: 730.0,
                height: 510.0,
                window: a,
            },
        );
        let _ = desktop.dispatch(a, Message::DisplayScale(2.0));
        let _ = desktop.dispatch(
            b,
            Message::WindowResized {
                width: 1100.0,
                height: 740.0,
                window: b,
            },
        );
        let wa = &desktop.windows[&a];
        let wb = &desktop.windows[&b];
        assert_eq!(wa.state.display_scale, 2.0);
        assert_eq!(wb.state.display_scale, 1.0);
        assert_eq!(wa.scene.as_ref().unwrap().style.metrics.font_size, 10.0);
        assert_eq!(wb.scene.as_ref().unwrap().style.metrics.font_size, 24.0);
        assert_ne!(wa.state.terminal_size, wb.state.terminal_size);
        assert_eq!(
            primary_size,
            (
                desktop.app.window_width,
                desktop.app.window_height,
                desktop.app.display_scale
            )
        );
        assert_eq!(desktop.app.manager.active_session(), primary_session);
        assert!(
            desktop.app.native_window_id == Some(desktop.primary) && !desktop.app.native_auxiliary
        );
    }

    #[test]
    fn inherited_style_changes_refit_the_attached_grid_before_the_next_view() {
        let (mut desktop, ids) = fixture();
        let _ = desktop.detach(ids[2]); // this profile inherits global appearance
        let child = desktop.owner(ids[2]);
        let old = desktop.windows[&child].state.terminal_size;
        let _ = desktop.dispatch(desktop.primary, Message::FontSizeStep(5));
        let window = &desktop.windows[&child];
        assert_eq!(window.scene.as_ref().unwrap().style.metrics.font_size, 18.0);
        assert_ne!(window.state.terminal_size.cols, old.cols);
        assert_eq!(
            desktop
                .app
                .manager
                .snapshot_for(ids[2], Viewport::tail(1))
                .size,
            window.state.terminal_size
        );
        assert_eq!(desktop.app.manager.sessions().len(), 3);
    }

    #[test]
    fn primary_resize_cannot_steal_the_only_detached_sessions_geometry() {
        let (mut desktop, ids) = fixture();
        desktop.app.manager.close(ids[1]);
        desktop.app.manager.close(ids[2]);
        desktop.app.manager.activate(ids[0]).unwrap();
        let _ = desktop.detach(ids[0]);
        let child = desktop.owner(ids[0]);
        let target = desktop.windows[&child].state.terminal_size;
        desktop.app.rdp_resize_pending = Some((321, 123));
        let _ = desktop.dispatch(
            desktop.primary,
            Message::WindowResized {
                width: 400.0,
                height: 300.0,
                window: desktop.primary,
            },
        );
        flush_pending_rdp_resize(&mut desktop.app);
        assert_eq!(desktop.app.rdp_resize_pending, None);
        assert_eq!(
            desktop
                .app
                .manager
                .snapshot_for(ids[0], Viewport::tail(1))
                .size,
            target
        );
        assert_eq!(desktop.app.manager.sessions().len(), 1);
        let _ = desktop.merge(child, false);
        assert_eq!(desktop.app.manager.sessions().len(), 1);
    }

    #[test]
    fn composition_and_keyboard_events_are_bound_to_window_session_and_focus_epoch() {
        let (mut desktop, ids) = fixture();
        let _ = desktop.detach(ids[0]);
        let a = desktop.owner(ids[0]);
        let _ = desktop.detach(ids[1]);
        let b = desktop.owner(ids[1]);
        desktop.focus(a);
        let epoch = desktop.windows[&a].state.vnc_input_epoch;
        let _ = desktop.dispatch(
            a,
            Message::NativeImeEvent(ids[0], epoch, Ime::Commit("first-window".into())),
        );
        assert!(text(&desktop, ids[0]).contains("first-window"));
        assert!(!text(&desktop, ids[1]).contains("first-window"));
        desktop.focus(b);
        let _ = desktop.dispatch(
            a,
            Message::NativeImeEvent(ids[0], epoch, Ime::Commit("stale-focus".into())),
        );
        let current = desktop.windows[&b].state.vnc_input_epoch;
        let _ = desktop.dispatch(
            b,
            Message::NativeImeEvent(ids[0], current, Ime::Commit("wrong-session".into())),
        );
        let _ = desktop.dispatch(
            b,
            Message::NativeImeEvent(ids[1], current, Ime::Commit("second-window".into())),
        );
        desktop.focus(a);
        let _ = desktop.dispatch(
            a,
            Message::NativeImeEvent(ids[0], epoch, Ime::Commit("old-epoch".into())),
        );
        for id in ids {
            let output = text(&desktop, id);
            for denied in ["stale-focus", "wrong-session", "old-epoch"] {
                assert!(!output.contains(denied));
            }
        }
        assert!(text(&desktop, ids[1]).contains("second-window"));
    }

    #[test]
    fn queued_surface_input_cannot_reuse_an_old_window_view_lease() {
        let (mut desktop, ids) = fixture();
        let _ = desktop.detach(ids[0]);
        let child = desktop.owner(ids[0]);
        desktop.focus(child);
        let old = desktop.windows[&child].state.vnc_input_epoch;
        desktop.focus(desktop.primary);
        desktop.focus(child);
        let current = desktop.windows[&child].state.vnc_input_epoch;
        assert_ne!(old, current);
        let _ = desktop.update(NativeMessage::View(
            child,
            Some(ids[0]),
            old,
            Message::TerminalPointerMoved(Point::new(40.0, 60.0)),
        ));
        assert!(desktop.windows[&child].state.terminal_pointer.is_none());
        let _ = desktop.update(NativeMessage::View(
            child,
            Some(ids[1]),
            current,
            Message::TerminalPointerMoved(Point::new(40.0, 60.0)),
        ));
        assert!(desktop.windows[&child].state.terminal_pointer.is_none());
        let _ = desktop.update(NativeMessage::View(
            child,
            Some(ids[0]),
            current,
            Message::TerminalPointerMoved(Point::new(40.0, 60.0)),
        ));
        assert!(desktop.windows[&child].state.terminal_pointer.is_some());
        assert_eq!(desktop.app.manager.sessions().len(), 3);
    }

    #[test]
    fn confirmation_activation_survives_an_unrelated_vnc_epoch_change() {
        let (mut desktop, ids) = fixture();
        let primary = desktop.primary;
        let _ = desktop.dispatch(
            primary,
            Message::OpenHyperlink("https://fixture.invalid".into()),
        );
        let generation = desktop.app.hyperlink_prompt_generation;
        let epoch = desktop.app.vnc_input_epoch;
        let revision = dialog_focus::FocusGroup::new().revision();
        let mut activation = None;
        assert!(revision.enqueue_activation(Some(0), true, |value| {
            activation = Some(value);
            true
        }));
        desktop.app.vnc_input_epoch = epoch.wrapping_add(1);
        let _ = desktop.update(NativeMessage::View(
            primary,
            Some(ids[2]),
            epoch,
            Message::DialogFocusActivate {
                activation,
                action: Box::new(Message::CancelOpenHyperlink(generation)),
                lease: None,
            },
        ));
        assert!(
            desktop.app.pending_hyperlink.is_none(),
            "an unrelated VNC input epoch invalidated the confirmation"
        );
    }

    #[cfg(windows)]
    #[test]
    fn confirmation_from_a_different_session_is_cancelled_and_can_be_retried() {
        let (mut desktop, ids) = fixture();
        let primary = desktop.primary;
        let _ = desktop.dispatch(
            primary,
            Message::OpenHyperlink("https://fixture.invalid".into()),
        );
        let generation = desktop.app.hyperlink_prompt_generation;
        let epoch = desktop.app.vnc_input_epoch;
        let revision = dialog_focus::FocusGroup::new().revision();
        let mut stale = None;
        assert!(revision.enqueue_activation(Some(0), true, |value| {
            stale = Some(value);
            true
        }));
        let stale = stale.unwrap();
        let _ = desktop.update(NativeMessage::View(
            primary,
            Some(ids[1]),
            epoch,
            Message::DialogFocusActivate {
                activation: Some(stale.clone()),
                action: Box::new(Message::CancelOpenHyperlink(generation)),
                lease: None,
            },
        ));
        assert!(desktop.app.pending_hyperlink.is_some());
        assert!(!stale.current());
        let mut fresh = None;
        assert!(revision.enqueue_activation(Some(0), true, |value| {
            fresh = Some(value);
            true
        }));
        let _ = desktop.update(NativeMessage::View(
            primary,
            Some(ids[2]),
            epoch,
            Message::DialogFocusActivate {
                activation: fresh,
                action: Box::new(Message::CancelOpenHyperlink(generation)),
                lease: None,
            },
        ));
        assert!(desktop.app.pending_hyperlink.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn modal_in_another_window_revokes_already_scheduled_focus() {
        use windows::{
            core::HSTRING,
            Win32::{
                Foundation::HINSTANCE,
                System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
                UI::WindowsAndMessaging::{
                    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW,
                    UnregisterClassW, WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPEDWINDOW,
                },
            },
        };
        unsafe extern "system" fn proc(
            hwnd: windows::Win32::Foundation::HWND,
            msg: u32,
            w: windows::Win32::Foundation::WPARAM,
            l: windows::Win32::Foundation::LPARAM,
        ) -> windows::Win32::Foundation::LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, w, l) }
        }
        let (mut desktop, ids) = fixture();
        let _ = desktop.detach(ids[0]);
        let child = desktop.owner(ids[0]);
        unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None).unwrap().0);
            let name = HSTRING::from(format!("Adit-SYNTHETIC-modal-{}", GetCurrentThreadId()));
            let class = WNDCLASSW {
                hInstance: instance,
                lpfnWndProc: Some(proc),
                lpszClassName: windows::core::PCWSTR(name.as_ptr()),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                &name,
                windows::core::w!("SYNTHETIC"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                200,
                100,
                None,
                None,
                Some(instance),
                None,
            )
            .unwrap();
            assert!(accessibility::test_register_window(
                desktop.primary,
                hwnd.0 as isize
            ));
            let epoch = accessibility::test_scope_epoch(desktop.primary);
            let scheduled = epoch.load(std::sync::atomic::Ordering::SeqCst);
            desktop.app.connection_dialog = Some(ConnectionDialog {
                profile_id: desktop
                    .app
                    .manager
                    .session_summary(ids[0])
                    .unwrap()
                    .profile_id,
                retry_session: Some(ids[0]),
                title: "SYNTHETIC".into(),
                endpoint: "fixture.invalid".into(),
                auth_method: AuthMethod::Password,
                identity_file: String::new(),
            });
            desktop.app.native_dialog_window = Some(child);
            desktop.sync_modal_owner();
            let revoked = epoch.load(std::sync::atomic::Ordering::SeqCst) != scheduled;
            accessibility::test_unregister_window(desktop.primary);
            DestroyWindow(hwnd).unwrap();
            UnregisterClassW(&name, Some(instance)).unwrap();
            assert!(
                revoked,
                "a focus task retained its lease after another window opened a modal"
            );
        }
    }

    #[test]
    fn explicitly_closed_connection_cannot_project_primarys_live_host_into_a_stale_window() {
        let (mut desktop, ids) = fixture();
        let _ = desktop.detach(ids[0]);
        let child = desktop.owner(ids[0]);
        desktop.app.manager.close(ids[0]);
        let before = text(&desktop, ids[2]);
        desktop.focus(child);
        let _ = desktop.dispatch(
            child,
            Message::NativeImeEvent(ids[2], 0, Ime::Commit("must-not-reach-primary".into())),
        );
        assert_eq!(text(&desktop, ids[2]), before);
        assert!(desktop.windows[&child].scene.is_none());
        assert!(desktop.title(child).contains("会话已关闭"));
        let _ = desktop.view(child);
        assert_eq!(desktop.app.manager.sessions().len(), 2);
        let _ = desktop.merge(child, false);
        assert_eq!(text(&desktop, ids[2]), before);
    }

    #[test]
    fn global_modal_can_be_cancelled_after_its_original_session_was_explicitly_closed() {
        let (mut desktop, ids) = fixture();
        let _ = desktop.detach(ids[0]);
        let child = desktop.owner(ids[0]);
        desktop.app.native_sftp_window = Some(child);
        desktop.app.native_dialog_window = Some(child);
        desktop.app.manager.close(ids[0]);
        let before = text(&desktop, ids[2]);
        let _ = desktop.dispatch(child, Message::CancelConnection);
        assert_ne!(desktop.app.native_sftp_window, Some(child));
        assert_ne!(desktop.app.native_dialog_window, Some(child));
        assert_eq!(text(&desktop, ids[2]), before);
        assert_eq!(desktop.app.manager.sessions().len(), 2);
    }

    #[test]
    fn focus_transfer_releases_all_held_input_and_late_unfocus_does_not_revoke_new_owner() {
        let (mut desktop, ids) = fixture();
        let _ = desktop.detach(ids[0]);
        let a = desktop.owner(ids[0]);
        let _ = desktop.detach(ids[1]);
        let b = desktop.owner(ids[1]);
        desktop.focus(a);
        let _ = desktop.in_window(a, |app| {
            app.rdp_pressed_keys.insert((0x1e, false)); // A, not just a modifier
            app.vnc_pressed_keys
                .push((keyboard::key::Physical::Code(keyboard::key::Code::KeyA), 97));
            app.mouse_button_down = true;
            app.terminal_selecting = true;
            Task::none()
        });
        desktop.focus(b);
        assert!(desktop.windows[&a].state.rdp_pressed_keys.is_empty());
        assert!(desktop.windows[&a].state.vnc_pressed_keys.is_empty());
        assert!(!desktop.windows[&a].state.mouse_button_down);
        assert!(!desktop.windows[&a].state.terminal_selecting);
        let epoch = desktop.windows[&b].state.vnc_input_epoch;
        let _ = desktop.update(NativeMessage::Runtime(
            a,
            iced::Event::Window(window::Event::Unfocused),
            event::Status::Ignored,
        ));
        assert_eq!(desktop.focused, Some(b));
        assert!(desktop.windows[&b].state.desktop_window_focused);
        assert_eq!(desktop.windows[&b].state.vnc_input_epoch, epoch);
    }

    #[test]
    fn prompts_follow_session_owner_and_merge_reattributes_global_services() {
        let (mut desktop, ids) = fixture();
        let _ = desktop.detach(ids[0]);
        let child = desktop.owner(ids[0]);
        assert!(!prompt_visible(&desktop.app, ids[0]));
        let mut visible = false;
        let _ = desktop.in_window(child, |app| {
            visible = prompt_visible(app, ids[0]);
            assert!(!prompt_visible(app, ids[2]));
            Task::none()
        });
        assert!(visible);
        desktop.app.native_sftp_window = Some(child);
        desktop.app.native_dialog_window = Some(child);
        assert!(!sftp_visible(&desktop.app));
        assert!(is_sftp(&Message::SftpUploadPicked(None)));
        let _ = desktop.merge(child, false);
        assert_eq!(desktop.app.native_sftp_window, Some(desktop.primary));
        assert_eq!(desktop.app.native_dialog_window, Some(desktop.primary));
        assert!(prompt_visible(&desktop.app, ids[0]));
        assert!(sftp_visible(&desktop.app));
        assert_eq!(desktop.app.manager.sessions().len(), 3);
        let before = text(&desktop, ids[0]);
        let _ = desktop.dispatch(
            child,
            Message::NativeImeEvent(ids[0], 0, Ime::Commit("closed-window".into())),
        );
        assert_eq!(text(&desktop, ids[0]), before);
    }

    #[test]
    fn merging_preserves_clipboard_stream_metadata_and_tab_switch_restores_its_owner() {
        let (mut desktop, ids) = fixture();
        let offered = |name: &str| clipboard_files::OfferedFile {
            path: std::path::PathBuf::from(name),
            meta: adit_session::ClipFile {
                name: name.into(),
                size: 1,
                is_dir: false,
            },
        };
        let primary_file = offered("fixture-only-primary.bin");
        let child_file = offered("fixture-only-child.bin");
        desktop.app.rdp_clipboard_offered = Some("primary-fixture-offer".into());
        desktop.app.rdp_offered_files = vec![primary_file.clone()];
        let _ = desktop.detach(ids[0]);
        let child = desktop.owner(ids[0]);
        let _ = desktop.in_window(child, |app| {
            app.rdp_clipboard_offered = Some("child-fixture-offer".into());
            app.rdp_offered_files = vec![child_file.clone()];
            Task::none()
        });
        let _ = desktop.merge(child, false);
        assert_eq!(
            desktop.app.rdp_clipboard_offered.as_deref(),
            Some("child-fixture-offer")
        );
        assert_eq!(desktop.app.rdp_offered_files, vec![child_file.clone()]);
        let _ = desktop.dispatch(desktop.primary, Message::TabClicked(ids[2]));
        assert_eq!(
            desktop.app.rdp_clipboard_offered.as_deref(),
            Some("primary-fixture-offer")
        );
        assert_eq!(desktop.app.rdp_offered_files, vec![primary_file]);
        let _ = desktop.dispatch(desktop.primary, Message::TabClicked(ids[0]));
        // Text anti-bounce state is intentionally reset on a tab switch; the
        // delayed file-content mapping must survive with its actual owner.
        assert_eq!(desktop.app.rdp_offered_files, vec![child_file]);
        assert_eq!(desktop.app.manager.sessions().len(), 3);
    }
}
