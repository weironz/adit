//! Live widget semantics. Native providers and actions are scoped to a window;
//! terminal content is data only, and protected controls never accept a value.
use super::*;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

thread_local! {
    static BUILD: RefCell<Option<String>> = const { RefCell::new(None) };
    static BUILD_WINDOW: Cell<Option<window::Id>> = const { Cell::new(None) };
}

pub(crate) struct Building(Option<String>, Option<window::Id>);
impl Drop for Building {
    fn drop(&mut self) {
        BUILD.with(|scope| *scope.borrow_mut() = self.0.take());
        BUILD_WINDOW.with(|window| window.set(self.1));
    }
}

pub(crate) fn building(window: Option<window::Id>, namespace: &str) -> Building {
    Building(
        BUILD.with(|scope| {
            scope
                .borrow_mut()
                .replace(format!("{window:?}/{namespace}"))
        }),
        BUILD_WINDOW.with(|current| current.replace(window)),
    )
}

fn input_id(label: &str) -> iced::advanced::widget::Id {
    BUILD.with(|scope| {
        iced::advanced::widget::Id::from(format!(
            "adit-uia:{}/field:{label}",
            scope.borrow().as_deref().unwrap_or("unregistered"),
        ))
    })
}

pub(crate) fn current_build_scope() -> String {
    BUILD.with(|scope| scope.borrow().clone().unwrap_or_default())
}

#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
mod acceptance;
#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub(crate) mod acceptance_guard;
#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub(crate) mod diagnostics {
    use std::sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex, OnceLock,
    };
    use std::time::Instant;

    static ENABLED: AtomicBool = AtomicBool::new(false);
    static KEYBOARD_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
    static FOREGROUND_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
    static TRACE: OnceLock<Mutex<(Instant, u64)>> = OnceLock::new();

    // Only the synthetic acceptance harness enables this. Callers supply fixed
    // event names, numeric IDs and enum state, never widget values or text.
    pub(crate) fn enable() {
        ENABLED.store(true, Ordering::SeqCst);
    }

    pub(crate) fn keyboard_attempt() {
        if ENABLED.load(Ordering::SeqCst) {
            KEYBOARD_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        }
    }
    pub(crate) fn keyboard_attempts() -> u64 {
        KEYBOARD_ATTEMPTS.load(Ordering::SeqCst)
    }
    pub(crate) fn foreground_attempt(accepted: bool) {
        if ENABLED.load(Ordering::SeqCst) {
            FOREGROUND_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
            event(format_args!(
                "scheduled-foreground-attempt accepted={accepted}"
            ));
        }
    }
    pub(crate) fn foreground_attempts() -> u64 {
        FOREGROUND_ATTEMPTS.load(Ordering::SeqCst)
    }

    pub(crate) fn event(args: std::fmt::Arguments<'_>) {
        if !ENABLED.load(Ordering::SeqCst) {
            return;
        }
        let mut trace = TRACE
            .get_or_init(|| Mutex::new((Instant::now(), 0)))
            .lock()
            .unwrap();
        trace.1 += 1;
        eprintln!(
            "SYNTHETIC-UIA seq={} us={} {args}",
            trace.1,
            trace.0.elapsed().as_micros()
        );
    }
}
#[cfg(windows)]
mod native;
#[cfg(all(windows, test))]
pub(crate) fn test_register_window(id: window::Id, hwnd: isize) -> bool {
    native::register(id, hwnd)
}
#[cfg(all(windows, test))]
pub(crate) fn test_unregister_window(id: window::Id) {
    native::unregister(id);
}
#[cfg(all(windows, test))]
pub(crate) fn test_scope_epoch(id: window::Id) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
    native::test_scope_epoch(id)
}
#[cfg(windows)]
mod widget;
#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub(super) use acceptance::run as run_acceptance;

#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub(crate) fn use_synthetic_fixture(app: &mut AditApp) {
    app.accessibility.fixture = Some(acceptance::Fixture::default());
}

#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub(crate) fn synthetic_conflicting_prompts(
    app: &mut AditApp,
    host_session: SessionId,
    auth_session: SessionId,
) -> (u64, u64) {
    let (host, host_receiver) = HostKeyPromptInfo::channel(
        "fixture.invalid".into(),
        22,
        "ssh-ed25519".into(),
        "SYNTHETIC".into(),
        None,
    );
    let (auth, auth_receiver) = AuthPromptInfo::channel(
        "SYNTHETIC authentication".into(),
        "".into(),
        vec![
            adit_session::AuthPromptField {
                prompt: "SYNTHETIC field 1".into(),
                echo: true,
            },
            adit_session::AuthPromptField {
                prompt: "SYNTHETIC field 2".into(),
                echo: false,
            },
        ],
    );
    let ids = (host.id, auth.id);
    let fixture = app
        .accessibility
        .fixture
        .as_mut()
        .expect("synthetic fixture");
    fixture.host_key = Some((host_session, host));
    fixture.receiver = Some(host_receiver);
    fixture.auth_prompt = Some((auth_session, auth.clone()));
    fixture.auth_receiver = Some(auth_receiver);
    app.auth_prompt = Some((auth_session, auth));
    app.auth_prompt_answers = vec![
        "SYNTHETIC-UIA-AUTH-FIRST".into(),
        "SYNTHETIC-UIA-AUTH-SECRET".into(),
    ];
    ids
}

#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub(crate) fn synthetic_cross_window_focus(
    window: window::Id,
    id: iced::advanced::widget::Id,
) -> Option<Task<Message>> {
    let lease = native::synthetic_cross_window_lease(window)?;
    Some(request_focus(window, id, None, Some(lease)))
}

#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub(crate) fn hold_native_actions(window: window::Id, hold: bool) {
    native::hold_actions(window, hold);
}

#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub(crate) fn native_focus_statistics(window: window::Id) -> (u64, u64) {
    native::focus_statistics(window)
}

#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub(crate) fn native_plain_click_statistics(window: window::Id) -> (u64, u64) {
    native::plain_click_statistics(window)
}

/// Captured when UIA asks for focus; revoked by a subsequent window switch.
#[derive(Clone, Debug)]
pub struct WindowFocusLease {
    #[cfg(windows)]
    state: std::sync::Arc<std::sync::Mutex<native::ForegroundState>>,
    #[cfg(windows)]
    generation: u64,
    #[cfg(windows)]
    foreground: isize,
    #[cfg(windows)]
    target: isize,
    #[cfg(windows)]
    scope_epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
    #[cfg(windows)]
    scope_generation: u64,
}

impl WindowFocusLease {
    pub(crate) fn current(&self) -> bool {
        #[cfg(windows)]
        {
            native::foreground_lease_current(self)
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

#[derive(Default)]
pub(crate) struct State {
    pub primary: Option<window::Id>,
    pub windows: HashSet<window::Id>,
    live_windows: HashSet<window::Id>,
    scrolls: HashMap<(window::Id, SessionId), usize>,
    /// Only the isolated acceptance runner sets this. Shipping instances use
    /// the actual visible control actions without a fixture policy.
    #[cfg(windows)]
    pub action_filter: Option<fn(&Message) -> bool>,
    #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
    fixture: Option<acceptance::Fixture>,
}

impl State {
    pub fn opening(&mut self, window: window::Id) -> bool {
        self.live_windows.insert(window);
        let added = self.windows.insert(window);
        if self.primary.is_none() && added {
            self.primary = Some(window);
        }
        added
    }

    pub fn closed(&mut self, window: window::Id) {
        self.live_windows.remove(&window);
        self.windows.remove(&window);
        self.scrolls.retain(|(owner, _), _| *owner != window);
        if self.primary == Some(window) {
            self.primary = None;
        }
        #[cfg(windows)]
        native::unregister(window);
    }

    pub fn registration_failed(&mut self, window: window::Id) {
        self.windows.remove(&window);
        #[cfg(windows)]
        native::unregister(window);
        // The native window still exists. Its identity is also needed to
        // restore terminal routing after an ordinary connection dialog.
    }
}

/// A UIA focus request temporarily owns a confirmation's keyboard focus.
/// The lease is projected with its window's presentation, never globally.
pub(crate) struct RemoteFocusLease {
    window: window::Id,
    session: SessionId,
    scope: String,
    terminal_was_focused: bool,
}

fn restorable_confirmation(scope: &str) -> bool {
    scope.starts_with("host-key-dialog:")
        || scope.starts_with("hyperlink:")
        || scope.starts_with("challenge:")
        || scope.starts_with("connection-dialog:")
        || scope == "native-modal-blocked"
}

pub(crate) fn suspend_remote_focus(app: &mut AditApp, window: window::Id) {
    let scope = namespace(app);
    if scope == "menu"
        && app
            .accessibility_focus_restore
            .as_ref()
            .is_some_and(|lease| {
                lease.window == window
                    && app.manager.active_session() == Some(lease.session)
                    && native_windows::owns_selected_session(app)
            })
    {
        app.terminal_focused = false;
        return;
    }
    let session = app
        .manager
        .active_session()
        .filter(|_| restorable_confirmation(&scope) && native_windows::owns_selected_session(app));
    if let Some(session) = session {
        match app.accessibility_focus_restore.as_mut() {
            Some(lease) if lease.window == window && lease.session == session => {
                lease.scope = scope;
            }
            _ => {
                app.accessibility_focus_restore = Some(RemoteFocusLease {
                    window,
                    session,
                    scope,
                    terminal_was_focused: app.terminal_focused,
                });
            }
        }
    } else {
        app.accessibility_focus_restore = None;
    }
    app.terminal_focused = false;
}

/// An Invoke can supersede its preceding UIA Focus before iced dispatches it.
/// Preserve and suspend remote input at commit for the still-visible dialog.
pub(crate) fn suspend_for_confirmation_action(app: &mut AditApp, action: &Message) {
    let scope = namespace(app);
    let matches_scope = match action {
        Message::RespondHostKey {
            session_id,
            challenge_id,
            ..
        } => scope == format!("host-key-dialog:{session_id}:{challenge_id}"),
        Message::ConfirmOpenHyperlink(generation) | Message::CancelOpenHyperlink(generation) => {
            scope == format!("hyperlink:{generation}")
        }
        Message::SubmitAuthPrompt { challenge_id, .. }
        | Message::CancelAuthPrompt { challenge_id, .. } => {
            scope == format!("challenge:{challenge_id}")
        }
        Message::ConfirmConnection | Message::CancelConnection => {
            scope.starts_with("connection-dialog:")
        }
        Message::ConfirmConnectionFor(id) | Message::CancelConnectionFor(id) => {
            scope == format!("connection-dialog:{id}")
        }
        _ => false,
    };
    if matches_scope {
        if let Some(window) = app.native_window_id.or(app.accessibility.primary) {
            suspend_remote_focus(app, window);
            release_rdp_modifiers(app);
        }
    }
}

pub(crate) fn reconcile_remote_focus(app: &mut AditApp) {
    let Some(mut lease) = app.accessibility_focus_restore.take() else {
        return;
    };
    if !app.accessibility.live_windows.contains(&lease.window)
        || app.native_window_id.or(app.accessibility.primary) != Some(lease.window)
        || app.manager.active_session() != Some(lease.session)
        || app.manager.session_summary(lease.session).is_none()
        || !native_windows::owns_selected_session(app)
    {
        return;
    }
    let scope = namespace(app);
    if scope == lease.scope || restorable_confirmation(&scope) || scope == "menu" {
        if scope != "menu" {
            lease.scope = scope;
        }
        app.accessibility_focus_restore = Some(lease);
    } else if scope == "main" && lease.terminal_was_focused && remote_workspace_eligible(app) {
        // This restores only local routing state. It never foregrounds a HWND
        // or revives a held key or an old IME composition.
        app.terminal_focused = true;
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Button,
    Dialog,
    Label,
    Item,
    Edit,
    Protected,
    Document,
    Range,
}

// No Debug implementation: even benign user-editable text can contain secrets.
#[derive(Clone)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct Control {
    key: String,
    label: String,
    kind: Kind,
    value: String,
    invoke: Option<Message>,
    set_value: Option<fn(String) -> Message>,
    range: Option<(Option<window::Id>, SessionId, usize, usize)>,
    focus: Option<iced::advanced::widget::Id>,
    focus_revision: Option<dialog_focus::FocusRevision>,
    focus_index: Option<usize>,
    labelled_by: Option<String>,
}

impl Control {
    pub fn button(key: impl Into<String>, label: impl Into<String>, message: Message) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            kind: Kind::Button,
            value: String::new(),
            invoke: Some(message),
            set_value: None,
            range: None,
            focus: None,
            focus_revision: None,
            focus_index: None,
            labelled_by: None,
        }
    }

    pub fn item(key: impl Into<String>, label: String, message: Message) -> Self {
        Self {
            kind: Kind::Item,
            ..Self::button(key, label, message)
        }
    }

    pub fn dialog(key: &str, label: &str) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            kind: Kind::Dialog,
            value: String::new(),
            invoke: None,
            set_value: None,
            range: None,
            focus: None,
            focus_revision: None,
            focus_index: None,
            labelled_by: None,
        }
    }

    pub fn label(key: &str, label: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            kind: Kind::Label,
            ..Self::dialog(key, "")
        }
    }

    pub fn read_value(mut self, value: String) -> Self {
        if matches!(self.kind, Kind::Item) {
            self.value = value;
        }
        self
    }
    pub fn edit(label: &'static str, value: &str, changed: fn(String) -> Message) -> Self {
        // This allowlist is intentional. Shell/ProxyCommand arguments, paths,
        // keys, environment values and authentication fields are not readable
        // merely because their on-screen widget happens to be a plain input.
        if !matches!(label, "名称" | "主机" | "端口" | "用户名" | "分组") {
            return Self::protected(label, !value.is_empty());
        }
        Self {
            key: format!("field:{label}"),
            label: t_in("field", label).into(),
            kind: Kind::Edit,
            value: value.chars().take(8192).collect(),
            invoke: None,
            set_value: Some(changed),
            range: None,
            focus: None,
            focus_revision: None,
            focus_index: None,
            labelled_by: None,
        }
    }

    pub fn protected(label: &str, occupied: bool) -> Self {
        Self {
            key: format!("protected:{label}"),
            label: t(label).into(),
            kind: Kind::Protected,
            // Fixed length: don't disclose the actual secret's length either.
            value: if occupied {
                "••••".into()
            } else {
                String::new()
            },
            invoke: None,
            set_value: None,
            range: None,
            focus: None,
            focus_revision: None,
            focus_index: None,
            labelled_by: None,
        }
    }

    pub fn document(owner: SessionId, snapshot: &TerminalSnapshot) -> Self {
        Self {
            key: format!("terminal:{owner}"),
            label: format!("{} — {}", t("终端"), snapshot.title),
            kind: Kind::Document,
            value: snapshot_to_text(snapshot).chars().take(65536).collect(),
            invoke: None,
            set_value: None,
            range: None,
            focus: None,
            focus_revision: None,
            focus_index: None,
            labelled_by: None,
        }
    }

    pub fn scroll(owner: SessionId, value: usize, max: usize) -> Self {
        Self {
            key: format!("terminal-scroll:{owner}"),
            label: t("终端滚动位置").into(),
            kind: Kind::Range,
            value: String::new(),
            invoke: None,
            set_value: None,
            range: Some((BUILD_WINDOW.with(Cell::get), owner, value.min(max), max)),
            focus: None,
            focus_revision: None,
            focus_index: None,
            labelled_by: None,
        }
    }
}

pub(crate) fn control<'a>(
    child: impl Into<Element<'a, Message>>,
    semantics: Control,
) -> Element<'a, Message> {
    #[cfg(windows)]
    {
        widget::control(child.into(), semantics)
    }
    #[cfg(not(windows))]
    {
        let _ = semantics;
        child.into()
    }
}

pub(crate) fn button<'a>(
    key: &str,
    label: &str,
    message: Message,
    child: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    control(child, Control::button(key, label, message))
}

/// Confirmation buttons share focus, starting on refusal for each generation.
/// Reverse navigation stops there instead of wrapping directly to trust/open.
pub(crate) fn confirmation_button<'a>(
    keys: &[&str],
    index: usize,
    group: &dialog_focus::FocusGroup,
    label: &str,
    message: Message,
    child: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let scope = BUILD.with(|scope| scope.borrow().clone().unwrap_or_default());
    let id = input_id(keys[index]);
    let mut metadata = Control::button(keys[index], label, message.clone());
    metadata.focus = Some(id.clone());
    control(
        dialog_focus::button(
            child.into(),
            dialog_focus::ButtonFocus {
                scope,
                id,
                index,
                count: keys.len(),
                group: group.clone(),
                action: message,
            },
        ),
        metadata,
    )
}

pub(crate) fn form_action_button<'a>(
    keys: &[&str],
    index: usize,
    group: &dialog_focus::FocusGroup,
    label: &str,
    message: Message,
    child: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let scope = BUILD.with(|scope| scope.borrow().clone().unwrap_or_default());
    let id = input_id(keys[index]);
    let mut metadata = Control::button(keys[index], label, message.clone());
    metadata.focus = Some(id.clone());
    control(
        dialog_focus::form_action(
            child.into(),
            dialog_focus::ButtonFocus {
                scope,
                id,
                index,
                count: keys.len(),
                group: group.clone(),
                action: message,
            },
        ),
        metadata,
    )
}

#[cfg(windows)]
fn window_scope_id(window: window::Id) -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::from(format!("adit-window:{window:?}"))
}

struct FocusIfPresent {
    target: iced::advanced::widget::Id,
    found: bool,
    confirmation: bool,
    ticket: Option<dialog_focus::FocusTicket>,
    lease: Option<WindowFocusLease>,
}

impl<T: 'static> iced::advanced::widget::Operation<T> for FocusIfPresent {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation<T>)) {
        operate(self);
    }

    fn focusable(
        &mut self,
        id: Option<&iced::advanced::widget::Id>,
        _bounds: iced::Rectangle,
        _state: &mut dyn iced::advanced::widget::operation::Focusable,
    ) {
        self.found |= id == Some(&self.target);
    }

    fn custom(
        &mut self,
        id: Option<&iced::advanced::widget::Id>,
        _bounds: iced::Rectangle,
        state: &mut dyn std::any::Any,
    ) {
        if id == Some(&self.target) && state.is::<dialog_focus::FocusState>() {
            self.found = true;
            self.confirmation = true;
        }
    }

    fn finish(&self) -> iced::advanced::widget::operation::Outcome<T> {
        use iced::advanced::widget::operation::Outcome;
        #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
        diagnostics::event(format_args!(
            "focus-operation found={} confirmation={} ticket={} lease_current={}",
            self.found,
            self.confirmation,
            self.ticket.is_some(),
            self.lease.as_ref().is_none_or(WindowFocusLease::current)
        ));
        if self.ticket.as_ref().is_some_and(|ticket| !ticket.current())
            || self.lease.as_ref().is_some_and(|lease| !lease.current())
        {
            if let Some(ticket) = &self.ticket {
                ticket.cancel();
            }
            Outcome::None
        } else if self.confirmation {
            Outcome::Chain(Box::new(FocusConfirmation {
                target: self.target.clone(),
                ticket: self.ticket.clone(),
                lease: self.lease.clone(),
                applied: false,
            }))
        } else if self.found && self.ticket.is_none() {
            Outcome::Chain(Box::new(GuardedFocus {
                target: self.target.clone(),
                lease: self.lease.clone(),
            }))
        } else {
            if let Some(ticket) = &self.ticket {
                ticket.cancel();
            }
            // A task queued for an old challenge must not unfocus the new
            // dialog's safe default when the old widget no longer exists.
            Outcome::None
        }
    }
}

struct FocusConfirmation {
    target: iced::advanced::widget::Id,
    ticket: Option<dialog_focus::FocusTicket>,
    lease: Option<WindowFocusLease>,
    applied: bool,
}

impl<T: 'static> iced::advanced::widget::Operation<T> for FocusConfirmation {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation<T>)) {
        operate(self);
    }

    fn custom(
        &mut self,
        id: Option<&iced::advanced::widget::Id>,
        _bounds: iced::Rectangle,
        state: &mut dyn std::any::Any,
    ) {
        if let (true, Some(button)) = (
            id == Some(&self.target),
            state.downcast_mut::<dialog_focus::FocusState>(),
        ) {
            self.applied |= button.focus_if_current(self.ticket.as_ref(), || {
                self.lease.as_ref().is_none_or(WindowFocusLease::current)
            });
            if self.lease.as_ref().is_some_and(|lease| !lease.current()) {
                if let Some(ticket) = &self.ticket {
                    ticket.cancel();
                }
            }
        }
    }

    fn finish(&self) -> iced::advanced::widget::operation::Outcome<T> {
        if !self.applied {
            if let Some(ticket) = &self.ticket {
                ticket.cancel();
            }
        }
        iced::advanced::widget::operation::Outcome::None
    }
}

struct GuardedFocus {
    target: iced::advanced::widget::Id,
    lease: Option<WindowFocusLease>,
}

impl<T: 'static> iced::advanced::widget::Operation<T> for GuardedFocus {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation<T>)) {
        operate(self);
    }

    fn focusable(
        &mut self,
        id: Option<&iced::advanced::widget::Id>,
        _bounds: iced::Rectangle,
        state: &mut dyn iced::advanced::widget::operation::Focusable,
    ) {
        if self.lease.as_ref().is_none_or(WindowFocusLease::current) {
            if id == Some(&self.target) {
                #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
                diagnostics::event(format_args!("focus-operation applied=true"));
                state.focus();
            } else {
                state.unfocus();
            }
        }
    }
}

#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub(crate) fn focus_widget(
    window: Option<window::Id>,
    id: iced::advanced::widget::Id,
) -> Task<Message> {
    focus_widget_with_ticket(window, id, None, None)
}

pub(crate) fn request_focus(
    window: window::Id,
    id: iced::advanced::widget::Id,
    ticket: Option<dialog_focus::FocusTicket>,
    lease: Option<WindowFocusLease>,
) -> Task<Message> {
    #[cfg(windows)]
    if let Some(lease) = lease {
        let check = ticket.clone();
        return window::run(window, move |_| {
            native::activate_for_focus(&lease, check.as_ref())
        })
        .then(move |fresh| {
            if let Some(fresh) = fresh {
                focus_widget_with_ticket(Some(window), id.clone(), ticket.clone(), Some(fresh))
            } else {
                if let Some(ticket) = &ticket {
                    ticket.cancel();
                }
                Task::none()
            }
        });
    }
    #[cfg(not(windows))]
    let _ = lease;
    focus_widget_with_ticket(Some(window), id, ticket, None)
}

pub(crate) fn focus_widget_with_ticket(
    window: Option<window::Id>,
    id: iced::advanced::widget::Id,
    ticket: Option<dialog_focus::FocusTicket>,
    lease: Option<WindowFocusLease>,
) -> Task<Message> {
    #[cfg(windows)]
    use iced::advanced::widget::operation;
    let focus = FocusIfPresent {
        target: id,
        found: false,
        confirmation: false,
        ticket,
        lease,
    };
    #[cfg(windows)]
    if let Some(window) = window {
        return iced::advanced::widget::operate(operation::scope(window_scope_id(window), focus));
    }
    let _ = window;
    iced::advanced::widget::operate(focus)
}

pub(crate) fn input<'a>(
    label: &'static str,
    value: &str,
    changed: fn(String) -> Message,
    child: iced::widget::TextInput<'a, Message>,
) -> Element<'a, Message> {
    let id = input_id(label);
    let mut metadata = Control::edit(label, value, changed);
    metadata.focus = Some(id.clone());
    control(child.id(id), metadata)
}

pub(crate) fn protected<'a>(
    label: &str,
    occupied: bool,
    child: iced::widget::TextInput<'a, Message>,
) -> Element<'a, Message> {
    let id = input_id(label);
    let mut metadata = Control::protected(label, occupied);
    metadata.focus = Some(id.clone());
    control(child.id(id), metadata)
}

pub(crate) fn protected_with_label<'a>(
    key: &str,
    label: &str,
    label_key: &str,
    occupied: bool,
    child: iced::widget::TextInput<'a, Message>,
) -> Element<'a, Message> {
    let id = input_id(key);
    let mut metadata = Control::protected(key, occupied);
    metadata.label = label.to_owned();
    metadata.labelled_by = Some(label_key.to_owned());
    metadata.focus = Some(id.clone());
    control(child.id(id), metadata)
}

pub(crate) fn scope<'a>(
    id: Option<window::Id>,
    namespace: String,
    child: Element<'a, Message>,
) -> Element<'a, Message> {
    #[cfg(windows)]
    {
        widget::scope(child, id, namespace)
    }
    #[cfg(not(windows))]
    {
        let _ = (id, namespace);
        child
    }
}

pub(crate) fn modal<'a>(
    child: Element<'a, Message>,
    escape: Option<Message>,
) -> Element<'a, Message> {
    let child = dialog_focus::shield(child, escape);
    #[cfg(windows)]
    {
        widget::modal(child)
    }
    #[cfg(not(windows))]
    {
        child
    }
}

pub(crate) fn register(id: window::Id) -> Task<Message> {
    #[cfg(windows)]
    {
        window::run(id, move |window| {
            use iced::window::raw_window_handle::RawWindowHandle;
            match window.window_handle().map(|h| h.as_raw()) {
                Ok(RawWindowHandle::Win32(handle)) => native::register(id, handle.hwnd.get()),
                _ => false,
            }
        })
        .map(move |attached| Message::AccessibilityReady {
            window: id,
            attached,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = id;
        Task::none()
    }
}

pub(crate) fn drain(app: &AditApp) -> Vec<(window::Id, String, Message)> {
    #[cfg(windows)]
    {
        native::drain(
            &app.accessibility.windows,
            app.native_window_id.or(app.accessibility.primary),
            &namespace(app),
        )
        .into_iter()
        .filter(|(_, _, message)| {
            let allowed = app
                .accessibility
                .action_filter
                .is_none_or(|allow| allow(message));
            if !allowed {
                cancel_discarded_message(message);
            }
            allowed
        })
        .collect()
    }
    #[cfg(not(windows))]
    {
        let _ = app;
        Vec::new()
    }
}

pub(crate) fn cancel_discarded_message(message: &Message) {
    match message {
        Message::AccessibilityFocus {
            ticket: Some(ticket),
            ..
        } => ticket.cancel(),
        Message::DialogFocusActivate {
            activation: Some(activation),
            ..
        } => activation.cancel(),
        _ => {}
    }
}

/// Revoke already scheduled UIA tasks before a modal in another HWND can
/// appear; waiting for this window's next redraw is too late.
pub(crate) fn block_background_window(window: window::Id) {
    #[cfg(windows)]
    native::retire_scope(window, "native-modal-blocked");
    #[cfg(not(windows))]
    let _ = window;
}

pub(crate) fn retire_changed_scope(app: &AditApp) {
    #[cfg(windows)]
    if let Some(window) = app.native_window_id.or(app.accessibility.primary) {
        native::retire_scope(window, &namespace(app));
    }
    #[cfg(not(windows))]
    let _ = app;
}

pub(crate) fn action_current(app: &AditApp, window: window::Id, scope: &str) -> bool {
    app.native_window_id.or(app.accessibility.primary) == Some(window)
        && app.accessibility.windows.contains(&window)
        && scope == namespace(app)
        && {
            #[cfg(windows)]
            {
                native::live(window)
            }
            #[cfg(not(windows))]
            {
                false
            }
        }
}

pub(crate) fn scroll(app: &mut AditApp, window: window::Id, owner: SessionId, requested: usize) {
    if !app.accessibility.windows.contains(&window) || app.manager.session_summary(owner).is_none()
    {
        return;
    }
    let rows = usize::from(session_terminal_size(app, owner).rows).max(1);
    let tail = app.manager.snapshot_for(owner, Viewport::tail(rows));
    let offset = requested.min(max_scroll_offset_for(&tail, rows));
    app.accessibility.scrolls.insert((window, owner), offset);
    // An action on another document cannot silently activate it or scroll the
    // active session. No terminal input, clipboard or remote command is sent.
    if app.manager.active_session() == Some(owner) {
        app.terminal_scroll_offset = offset;
    }
}

pub(crate) fn scroll_offset(app: &AditApp, owner: SessionId) -> usize {
    app.native_window_id
        .or(app.accessibility.primary)
        .and_then(|window| app.accessibility.scrolls.get(&(window, owner)).copied())
        .unwrap_or(0)
}

pub(crate) fn scroll_lines(app: &mut AditApp, window: window::Id, owner: SessionId, lines: i32) {
    let current = if app.native_window_id.or(app.accessibility.primary) == Some(window)
        && app.manager.active_session() == Some(owner)
    {
        app.terminal_scroll_offset
    } else {
        app.accessibility
            .scrolls
            .get(&(window, owner))
            .copied()
            .unwrap_or(0)
    };
    let next = if lines >= 0 {
        current.saturating_add(lines as usize)
    } else {
        current.saturating_sub(lines.unsigned_abs() as usize)
    };
    scroll(app, window, owner, next);
}

/// These are actual visible buttons over the existing full-height rail.
/// AccessKit treats `ScrollBar` ranges as read-only on Windows, so the numeric
/// position is exposed as a slider while the visible rail and step buttons stay
/// the real controls.
pub(crate) fn scroll_controls(
    owner: SessionId,
    offset: usize,
    max: usize,
    rail: Element<'static, Message>,
) -> Element<'static, Message> {
    let Some(window) = BUILD_WINDOW.with(Cell::get) else {
        return rail;
    };
    if max == 0 {
        return rail;
    }
    let step = |up: bool| {
        let message = Message::AccessibilityScrollLines {
            window,
            owner,
            lines: if up { 3 } else { -3 },
        };
        let label = if up {
            t("向上滚动终端")
        } else {
            t("向下滚动终端")
        };
        let enabled = if up { offset < max } else { offset > 0 };
        let mut button = iced::widget::button(text(if up { "↑" } else { "↓" }).size(12))
            .width(SCROLLBAR_WIDTH)
            .height(24)
            .padding(0)
            .style(|_, status| secondary_button_style(status));
        let mut semantics = Control::button(
            format!("terminal-{}:{owner}", if up { "up" } else { "down" }),
            label,
            message.clone(),
        );
        if enabled {
            button = button.on_press(message);
        } else {
            semantics.invoke = None;
        }
        control(button, semantics)
    };
    stack![
        rail,
        iced::widget::column![step(true), Space::new().height(Fill), step(false)]
            .width(SCROLLBAR_WIDTH)
            .height(Fill)
    ]
    .into()
}

pub(crate) fn pending_host_key(app: &AditApp) -> Option<(SessionId, HostKeyPromptInfo)> {
    let prompt = app.manager.pending_host_key();
    #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
    let prompt = prompt.or_else(|| {
        app.accessibility
            .fixture
            .as_ref()?
            .host_key
            .as_ref()
            .filter(|(_, prompt)| prompt.is_pending())
            .cloned()
    });
    prompt
}

pub(crate) fn pending_auth_prompt(app: &AditApp) -> Option<(SessionId, AuthPromptInfo)> {
    let prompt = app.manager.pending_auth_prompt();
    #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
    let prompt = prompt.or_else(|| {
        app.accessibility
            .fixture
            .as_ref()?
            .auth_prompt
            .as_ref()
            .filter(|(_, prompt)| prompt.is_pending())
            .cloned()
    });
    prompt
}

#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub(crate) fn capture_auth_response(app: &mut AditApp, challenge: u64) {
    if let Some(fixture) = app.accessibility.fixture.as_mut() {
        if let Some(answers) = fixture
            .auth_receiver
            .as_mut()
            .and_then(|receiver| receiver.try_recv().ok())
        {
            fixture.auth_results.push((challenge, answers));
        }
    }
}

#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub(crate) fn acceptance_confirm_connection(app: &mut AditApp) -> bool {
    let Some(fixture) = app.accessibility.fixture.as_mut() else {
        return false;
    };
    if app.connection_dialog.is_some() {
        fixture.connection_confirms += 1;
        app.connection_dialog = None;
        app.password.clear();
    }
    true
}

#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub(crate) fn acceptance_host_key_reply(
    app: &mut AditApp,
    session: SessionId,
    challenge: u64,
    decision: adit_session::HostKeyDecision,
) -> bool {
    let Some(fixture) = app.accessibility.fixture.as_mut() else {
        return false;
    };
    if fixture.host_key.as_ref().is_some_and(|(owner, prompt)| {
        *owner == session && prompt.id == challenge && prompt.respond(decision)
    }) && fixture
        .receiver
        .as_mut()
        .and_then(|receiver| receiver.try_recv().ok())
        == Some(decision)
    {
        fixture.decisions.push((challenge, decision));
        diagnostics::event(format_args!(
            "decision challenge={challenge} value={decision:?} count={}",
            fixture.decisions.len()
        ));
    }
    true
}

#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub(crate) fn acceptance_open_link(app: &mut AditApp, url: &str) -> bool {
    let Some(fixture) = app.accessibility.fixture.as_mut() else {
        return false;
    };
    fixture.opened_links.push(url.into());
    true
}

pub(crate) fn namespace(app: &AditApp) -> String {
    if let Some(window) = app.native_window_id.filter(|_| app.native_auxiliary) {
        return detached_namespace(
            app,
            window,
            app.pending_hyperlink.is_some(),
            app.hyperlink_prompt_generation,
            app.paste_confirm_open,
            app.terminal_context_menu,
        );
    }
    let global = native_windows::global_modal(app);
    if app.native_window_id.is_some_and(|window| {
        global
            .and_then(|modal| native_windows::global_modal_owner(app, modal))
            .is_some_and(|owner| owner != window)
    }) {
        return "native-modal-blocked".into();
    }
    // Same topmost-modal order as chrome::view. Hidden editor/navigation
    // actions cannot run during a newer dialog, even before the next redraw.
    if app.known_hosts_open {
        return "known-hosts".into();
    }
    if app.settings_open {
        return "settings".into();
    }
    if app.snippets_open {
        return "snippets".into();
    }
    if app.renaming_session.is_some() {
        return "rename".into();
    }
    if app.paste_confirm_open {
        return "paste".into();
    }
    if app.update_dialog_open {
        return "update".into();
    }
    if app.about_open {
        return "about".into();
    }
    if app.tunnels_open {
        return "tunnels".into();
    }
    if global == Some(native_windows::GlobalModal::Sftp) {
        return "sftp".into();
    }
    if app.pending_hyperlink.is_some() {
        return format!("hyperlink:{}", app.hyperlink_prompt_generation);
    }
    if let Some((session, prompt)) = &app.auth_prompt {
        if global == Some(native_windows::GlobalModal::Authentication(*session)) {
            return format!("challenge:{}", prompt.id);
        }
    }
    if let Some((session, prompt)) = pending_host_key(app) {
        if global == Some(native_windows::GlobalModal::HostKey(session)) {
            return format!("host-key-dialog:{session}:{}", prompt.id);
        }
    }
    if global == Some(native_windows::GlobalModal::Connection) {
        return format!(
            "connection-dialog:{}",
            app.connection_dialog.as_ref().unwrap().id
        );
    }
    if let Some(owner) = app.profile_editor {
        return format!("editor:{owner}");
    }
    if app.active_menu.is_some()
        || app.profile_context_menu.is_some()
        || app.tab_context_menu.is_some()
        || app.group_context_menu.is_some()
        || app.move_to_group_menu
        || app.terminal_context_menu
    {
        return "menu".into();
    }
    "main".into()
}

/// Mirrors the top layer of a detached window, before and after projecting its
/// presentation into AditApp. A global modal in another window revokes old
/// routes immediately, even before this window repaints its blocking scrim.
pub(crate) fn detached_namespace(
    app: &AditApp,
    window: window::Id,
    link: bool,
    link_generation: u64,
    paste: bool,
    context_menu: bool,
) -> String {
    let global = native_windows::global_modal(app);
    let modal_owner = global.and_then(|modal| native_windows::global_modal_owner(app, modal));
    if modal_owner.is_some_and(|owner| owner != window) {
        return "native-modal-blocked".into();
    }
    if paste {
        return "paste".into();
    }
    if link {
        return format!("hyperlink:{link_generation}");
    }
    if let Some((session, prompt)) = &app.auth_prompt {
        if global == Some(native_windows::GlobalModal::Authentication(*session)) {
            return format!("challenge:{}", prompt.id);
        }
    }
    if let Some((session, prompt)) = pending_host_key(app) {
        if global == Some(native_windows::GlobalModal::HostKey(session)) {
            return format!("host-key-dialog:{session}:{}", prompt.id);
        }
    }
    if global == Some(native_windows::GlobalModal::Connection) {
        return format!(
            "connection-dialog:{}",
            app.connection_dialog.as_ref().unwrap().id
        );
    }
    if global == Some(native_windows::GlobalModal::Sftp) {
        return "sftp".into();
    }
    if context_menu {
        return "menu".into();
    }
    "main".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn focused_action(message: &Message) -> &Message {
        match message {
            Message::DialogFocusActivate { action, .. } => action,
            _ => message,
        }
    }

    #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
    #[test]
    fn simultaneous_host_key_and_mfa_choose_one_actionable_owner() {
        let (mut app, primary, host_session, auth_session) = focus_fixture();
        let child = window::Id::unique();
        app.native_session_windows.insert(auth_session, child);
        let (host, host_rx) = HostKeyPromptInfo::channel(
            "fixture.invalid".into(),
            22,
            "ssh-ed25519".into(),
            "SYNTHETIC".into(),
            None,
        );
        let host_id = host.id;
        let (auth, _auth_rx) = AuthPromptInfo::channel("SYNTHETIC".into(), "".into(), Vec::new());
        let auth_id = auth.id;
        app.accessibility.fixture = Some(acceptance::Fixture {
            host_key: Some((host_session, host.clone())),
            receiver: Some(host_rx),
            ..Default::default()
        });
        app.auth_prompt = Some((auth_session, auth));
        assert_eq!(
            namespace(&app),
            format!("host-key-dialog:{host_session}:{host_id}"),
            "the selected host-key owner was blocked by another window's MFA prompt"
        );
        assert_eq!(
            detached_namespace(&app, child, false, 0, false, false),
            "native-modal-blocked"
        );
        assert!(host.respond(adit_session::HostKeyDecision::Reject));
        assert_eq!(namespace(&app), "native-modal-blocked");
        assert_eq!(
            detached_namespace(&app, child, false, 0, false, false),
            format!("challenge:{auth_id}")
        );
        assert_eq!(app.native_window_id, Some(primary));
    }

    #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
    #[test]
    fn host_key_precedes_auth_in_same_or_other_window_then_auth_resumes() {
        for separate in [false, true] {
            let (mut app, primary, auth_session, host_session) = focus_fixture();
            let host_window = if separate {
                window::Id::unique()
            } else {
                primary
            };
            if separate {
                app.native_session_windows.insert(host_session, host_window);
            }
            let (host, host_rx) = HostKeyPromptInfo::channel(
                "fixture.invalid".into(),
                22,
                "ssh-ed25519".into(),
                "SYNTHETIC".into(),
                None,
            );
            let (auth, _auth_rx) =
                AuthPromptInfo::channel("SYNTHETIC".into(), "".into(), Vec::new());
            let (host_id, auth_id) = (host.id, auth.id);
            app.accessibility.fixture = Some(acceptance::Fixture {
                host_key: Some((host_session, host.clone())),
                receiver: Some(host_rx),
                ..Default::default()
            });
            app.auth_prompt = Some((auth_session, auth));
            if separate {
                assert_eq!(namespace(&app), "native-modal-blocked");
                assert_eq!(
                    detached_namespace(&app, host_window, false, 0, false, false),
                    format!("host-key-dialog:{host_session}:{host_id}")
                );
            } else {
                assert_eq!(
                    namespace(&app),
                    format!("host-key-dialog:{host_session}:{host_id}")
                );
            }
            let _ = update_loop::update(
                &mut app,
                Message::CancelAuthPrompt {
                    session_id: auth_session,
                    challenge_id: auth_id,
                },
            );
            assert!(app
                .auth_prompt
                .as_ref()
                .is_some_and(|(_, prompt)| prompt.id == auth_id && prompt.is_pending()));
            assert!(host.respond(adit_session::HostKeyDecision::Reject));
            assert_eq!(namespace(&app), format!("challenge:{auth_id}"));
        }
    }

    fn focus_fixture() -> (AditApp, window::Id, SessionId, SessionId) {
        let profiles = vec![
            ConnectionProfile::new("one", "fixture.invalid", 22, "fixture"),
            ConnectionProfile::new("two", "fixture.invalid", 22, "fixture"),
        ];
        let ids: Vec<_> = profiles.iter().map(|profile| profile.id).collect();
        let mut manager = SessionManager::with_profiles(profiles);
        let one = manager.open_mock_session(ids[0]).unwrap();
        let two = manager.open_mock_session(ids[1]).unwrap();
        manager.activate(one).unwrap();
        let window = window::Id::unique();
        let mut app = AditApp {
            manager,
            main_view: MainView::Terminal,
            native_main_window: Some(window),
            native_window_id: Some(window),
            terminal_focused: true,
            desktop_window_focused: true,
            ..AditApp::default()
        };
        app.accessibility.opening(window);
        (app, window, one, two)
    }

    fn focus_link(app: &mut AditApp, window: window::Id) -> (u64, u64) {
        let _ = update_loop::update(
            app,
            Message::OpenHyperlink("https://fixture.invalid".into()),
        );
        let generation = app.hyperlink_prompt_generation;
        let old_epoch = app.vnc_input_epoch;
        let _ = update_loop::update(
            app,
            Message::AccessibilityFocus {
                window,
                id: iced::advanced::widget::Id::from("synthetic-confirmation"),
                ticket: None,
                lease: None,
            },
        );
        assert!(!app.terminal_focused);
        (generation, old_epoch)
    }

    #[test]
    fn uia_confirmation_restores_only_same_session_and_fresh_input() {
        let (mut app, window, session, _) = focus_fixture();
        let (generation, old_epoch) = focus_link(&mut app, window);
        let _ = update_loop::update(&mut app, Message::CancelOpenHyperlink(generation));
        assert!(app.terminal_focused);
        assert!(remote_workspace_accepts_input(&app));
        let before = snapshot_to_text(&app.manager.snapshot_for(session, Viewport::visible(200)));
        let _ = update_loop::update(
            &mut app,
            Message::NativeImeEvent(
                session,
                old_epoch,
                iced::advanced::input_method::Event::Commit("STALE".into()),
            ),
        );
        assert_eq!(
            snapshot_to_text(&app.manager.snapshot_for(session, Viewport::visible(200))),
            before
        );
        let current_epoch = app.vnc_input_epoch;
        let _ = update_loop::update(
            &mut app,
            Message::NativeImeEvent(
                session,
                current_epoch,
                iced::advanced::input_method::Event::Commit("SYNTHETIC-FRESH-IME".into()),
            ),
        );
        assert!(
            snapshot_to_text(&app.manager.snapshot_for(session, Viewport::visible(200)))
                .contains("SYNTHETIC-FRESH-IME")
        );
        let after_ime =
            snapshot_to_text(&app.manager.snapshot_for(session, Viewport::visible(200)));
        let key = Key::Character("q".into());
        let _ = update_loop::update(
            &mut app,
            Message::KeyboardInput(keyboard::Event::KeyPressed {
                key: key.clone(),
                modified_key: key,
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyQ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: Some("q".into()),
                repeat: false,
            }),
        );
        assert_ne!(
            snapshot_to_text(&app.manager.snapshot_for(session, Viewport::visible(200))),
            after_ime
        );
    }

    #[test]
    fn uia_focus_restore_does_not_steal_another_session_window_or_local_field() {
        let (mut app, window, _, other) = focus_fixture();
        let (generation, _) = focus_link(&mut app, window);
        app.manager.activate(other).unwrap();
        let _ = update_loop::update(&mut app, Message::ClearError);
        let _ = update_loop::update(&mut app, Message::CancelOpenHyperlink(generation));
        assert!(!app.terminal_focused);
        assert_eq!(app.manager.active_session(), Some(other));

        let (mut app, window, _, _) = focus_fixture();
        app.terminal_focused = false;
        let (generation, _) = focus_link(&mut app, window);
        let _ = update_loop::update(&mut app, Message::CancelOpenHyperlink(generation));
        assert!(!app.terminal_focused);

        let (mut app, window, _, _) = focus_fixture();
        let (generation, _) = focus_link(&mut app, window);
        app.desktop_window_focused = false;
        let _ = update_loop::update(&mut app, Message::CancelOpenHyperlink(generation));
        assert!(app.terminal_focused);
        assert!(!remote_workspace_accepts_input(&app));
        app.desktop_window_focused = true;
        assert!(remote_workspace_accepts_input(&app));

        let (mut app, window, _, _) = focus_fixture();
        let (generation, _) = focus_link(&mut app, window);
        app.search_open = true;
        let _ = update_loop::update(&mut app, Message::CancelOpenHyperlink(generation));
        assert!(!app.terminal_focused);
        assert!(app.search_open);

        let (mut app, window, _, _) = focus_fixture();
        let (generation, _) = focus_link(&mut app, window);
        app.native_window_id = Some(window::Id::unique());
        let _ = update_loop::update(&mut app, Message::CancelOpenHyperlink(generation));
        assert!(!app.terminal_focused);

        let (mut app, window, _, _) = focus_fixture();
        let (generation, _) = focus_link(&mut app, window);
        app.accessibility.closed(window);
        let _ = update_loop::update(&mut app, Message::CancelOpenHyperlink(generation));
        assert!(!app.terminal_focused);
    }

    #[test]
    fn another_windows_modal_defers_focus_restore_for_the_same_session() {
        let (mut app, window, session, _) = focus_fixture();
        let (generation, _) = focus_link(&mut app, window);
        let profile = app.manager.session_summary(session).unwrap().profile_id;
        app.native_dialog_window = Some(window::Id::unique());
        app.connection_dialog = Some(ConnectionDialog {
            id: next_connection_dialog_id(),
            profile_id: profile,
            retry_session: None,
            title: "synthetic".into(),
            endpoint: "fixture.invalid".into(),
            auth_method: AuthMethod::Password,
            identity_file: String::new(),
        });
        let _ = update_loop::update(&mut app, Message::ClearError);
        assert_eq!(namespace(&app), "native-modal-blocked");
        assert!(!app.terminal_focused);
        app.connection_dialog = None;
        let _ = update_loop::update(&mut app, Message::CancelOpenHyperlink(generation));
        assert!(app.terminal_focused);
        assert_eq!(app.manager.active_session(), Some(session));
    }

    #[test]
    fn old_connection_actions_cannot_operate_on_a_replacement_dialog() {
        let (mut app, _, session, _) = focus_fixture();
        let profile_id = app.manager.session_summary(session).unwrap().profile_id;
        let old_id = next_connection_dialog_id();
        let new_id = next_connection_dialog_id();
        app.connection_dialog = Some(ConnectionDialog {
            id: new_id,
            profile_id,
            retry_session: None,
            title: "SYNTHETIC".into(),
            endpoint: "fixture.invalid".into(),
            auth_method: AuthMethod::Password,
            identity_file: String::new(),
        });
        app.password = "SYNTHETIC-PASSWORD".into();
        let _ = update_loop::update(&mut app, Message::CancelConnectionFor(old_id));
        let _ = update_loop::update(&mut app, Message::ConfirmConnectionFor(old_id));
        assert_eq!(
            app.connection_dialog.as_ref().map(|dialog| dialog.id),
            Some(new_id)
        );
        assert_eq!(app.password, "SYNTHETIC-PASSWORD");
    }

    #[test]
    fn menu_defers_confirmation_focus_restore_until_it_is_dismissed() {
        let (mut app, window, session, _) = focus_fixture();
        app.active_menu = Some(MenuKind::File);
        let (generation, _) = focus_link(&mut app, window);
        let _ = update_loop::update(&mut app, Message::CancelOpenHyperlink(generation));
        assert_eq!(namespace(&app), "menu");
        assert!(!app.terminal_focused);
        assert!(app.accessibility_focus_restore.is_some());
        let _ = update_loop::update(
            &mut app,
            Message::AccessibilityFocus {
                window,
                id: iced::advanced::widget::Id::from("synthetic-menu-item"),
                ticket: None,
                lease: None,
            },
        );
        assert!(app.accessibility_focus_restore.is_some());
        let _ = update_loop::update(
            &mut app,
            Message::DismissMenu(MenuEscapeTarget::Active(MenuKind::File)),
        );
        assert!(app.terminal_focused);
        assert_eq!(app.manager.active_session(), Some(session));
        assert!(app.accessibility_focus_restore.is_none());
    }

    #[test]
    fn opening_connection_dialog_preserves_the_previous_focus_before_uia() {
        let (mut app, window, session, _) = focus_fixture();
        let profile = app.manager.session_summary(session).unwrap().profile_id;
        select_profile(&mut app, profile);
        app.terminal_focused = true;
        open_connection_dialog(&mut app);
        assert_eq!(
            namespace(&app),
            format!(
                "connection-dialog:{}",
                app.connection_dialog.as_ref().unwrap().id
            )
        );
        assert!(!app.terminal_focused);
        let _ = update_loop::update(
            &mut app,
            Message::AccessibilityFocus {
                window,
                id: iced::advanced::widget::Id::from("synthetic-password"),
                ticket: None,
                lease: None,
            },
        );
        let _ = update_loop::update(&mut app, Message::CancelConnection);
        assert!(app.terminal_focused);
        assert_eq!(app.manager.active_session(), Some(session));

        app.terminal_focused = false;
        open_connection_dialog(&mut app);
        let _ = update_loop::update(&mut app, Message::CancelConnection);
        assert!(!app.terminal_focused);

        app.terminal_focused = true;
        open_password_reprompt(&mut app, session, profile, "synthetic refusal");
        assert!(!app.terminal_focused);
        let _ = update_loop::update(&mut app, Message::CancelConnection);
        assert!(app.terminal_focused);
    }

    #[cfg(windows)]
    #[test]
    fn invoke_without_a_dispatched_focus_still_suspends_and_restores_remote_input() {
        let (mut app, window, session, _) = focus_fixture();
        let _ = update_loop::update(
            &mut app,
            Message::OpenHyperlink("https://fixture.invalid".into()),
        );
        let generation = app.hyperlink_prompt_generation;
        app.active_menu = Some(MenuKind::File);
        let revision = dialog_focus::FocusGroup::new().revision();
        let mut activation = None;
        assert!(revision.enqueue_activation(Some(0), true, |value| {
            activation = Some(value);
            true
        }));
        let _ = update_loop::update(
            &mut app,
            Message::DialogFocusActivate {
                activation,
                action: Box::new(Message::CancelOpenHyperlink(generation)),
                lease: None,
            },
        );
        assert!(app.pending_hyperlink.is_none());
        assert!(
            !app.terminal_focused,
            "Invoke closed the modal without suspending remote focus under the menu"
        );
        assert!(app.accessibility_focus_restore.is_some());
        let _ = update_loop::update(
            &mut app,
            Message::DismissMenu(MenuEscapeTarget::Active(MenuKind::File)),
        );
        assert!(app.terminal_focused);
        assert_eq!(app.manager.active_session(), Some(session));
        assert_eq!(app.native_window_id, Some(window));
    }

    #[cfg(windows)]
    #[test]
    fn stale_confirmation_invoke_does_not_suspend_the_replacement_scope() {
        let (mut app, _, _, _) = focus_fixture();
        let _ = update_loop::update(
            &mut app,
            Message::OpenHyperlink("https://fixture.invalid/one".into()),
        );
        let old = app.hyperlink_prompt_generation;
        let _ = update_loop::update(
            &mut app,
            Message::OpenHyperlink("https://fixture.invalid/two".into()),
        );
        let revision = dialog_focus::FocusGroup::new().revision();
        let mut activation = None;
        assert!(revision.enqueue_activation(Some(0), true, |value| {
            activation = Some(value);
            true
        }));
        let _ = update_loop::update(
            &mut app,
            Message::DialogFocusActivate {
                activation,
                action: Box::new(Message::CancelOpenHyperlink(old)),
                lease: None,
            },
        );
        assert_eq!(
            app.pending_hyperlink.as_deref(),
            Some("https://fixture.invalid/two")
        );
        assert!(app.terminal_focused);
        assert!(app.accessibility_focus_restore.is_none());
    }

    #[test]
    fn connection_cancel_restores_terminal_after_uia_registration_fails() {
        for native_window in [true, false] {
            let (mut app, window, session, _) = focus_fixture();
            if !native_window {
                app.native_window_id = None;
                app.native_main_window = None;
            }
            let profile = app.manager.session_summary(session).unwrap().profile_id;
            select_profile(&mut app, profile);
            app.terminal_focused = true;
            let _ = update_loop::update(
                &mut app,
                Message::AccessibilityReady {
                    window,
                    attached: false,
                },
            );
            assert!(!app.accessibility.windows.contains(&window));
            assert_eq!(app.accessibility.primary, Some(window));
            open_connection_dialog(&mut app);
            assert!(!app.terminal_focused);
            let _ = update_loop::update(&mut app, Message::CancelConnection);
            assert!(app.terminal_focused);
            assert_eq!(app.manager.active_session(), Some(session));
        }
    }

    #[test]
    fn iced_event_batch_invalidates_already_dispatched_uia_focus() {
        use adit_session::HostKeyDecision;
        use iced::advanced::widget::{operation::Outcome, Operation};
        use iced_runtime::user_interface::{Cache, UserInterface};

        let _scope = building(Some(window::Id::unique()), "host-key-dialog:fixture");
        let group = dialog_focus::FocusGroup::new();
        let keys = ["reject", "reject-record", "once", "trust"];
        let decisions = [
            HostKeyDecision::Reject,
            HostKeyDecision::RejectAndRecord,
            HostKeyDecision::TrustOnce,
            HostKeyDecision::TrustAndRecord,
        ];
        let mut row = iced::widget::Row::new();
        for (index, decision) in decisions.iter().enumerate() {
            row = row.push(confirmation_button(
                &keys,
                index,
                &group,
                keys[index],
                Message::RespondHostKey {
                    session_id: SessionId::new(),
                    challenge_id: 1,
                    decision: *decision,
                },
                Space::new().width(80).height(40),
            ));
        }
        let mut renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(
            iced::Font::default(),
            iced::Pixels(16.0),
        ));
        let mut ui = UserInterface::build(
            row,
            iced::Size::new(400.0, 100.0),
            Cache::new(),
            &mut renderer,
        );
        let mut clipboard = iced::advanced::clipboard::Null;
        let mut messages = Vec::new();
        let ticket = group.revision().issue();
        let mut discovery = FocusIfPresent {
            target: input_id("trust"),
            found: false,
            confirmation: false,
            ticket: Some(ticket.clone()),
            lease: None,
        };
        ui.operate(&renderer, &mut discovery);
        let Outcome::Chain(mut delayed) = <FocusIfPresent as Operation<()>>::finish(&discovery)
        else {
            panic!("the queued UIA focus should find the trust button");
        };
        let key = |named, code| {
            let key = Key::Named(named);
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: key.clone(),
                modified_key: key,
                physical_key: keyboard::key::Physical::Code(code),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            })
        };
        let tab = key(Named::Tab, keyboard::key::Code::Tab);
        let (_, statuses) = ui.update(
            &[tab.clone(), tab.clone(), tab.clone(), tab],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard,
            &mut messages,
        );
        assert!(statuses
            .iter()
            .all(|status| *status == event::Status::Captured));
        assert!(!ticket.current());
        ui.operate(&renderer, delayed.as_mut());
        ui.update(
            &[key(Named::Enter, keyboard::key::Code::Enter)],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard,
            &mut messages,
        );
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            focused_action(&messages[0]),
            Message::RespondHostKey {
                decision: HostKeyDecision::Reject,
                ..
            }
        ));

        messages.clear();
        ui.operate(
            &renderer,
            &mut FocusConfirmation {
                target: input_id("trust"),
                ticket: None,
                lease: None,
                applied: false,
            },
        );
        let ticket = group.revision().issue();
        let mut discovery = FocusIfPresent {
            target: input_id("trust"),
            found: false,
            confirmation: false,
            ticket: Some(ticket.clone()),
            lease: None,
        };
        ui.operate(&renderer, &mut discovery);
        let Outcome::Chain(mut delayed) = <FocusIfPresent as Operation<()>>::finish(&discovery)
        else {
            panic!("the queued UIA focus should find the trust button");
        };
        ui.update(
            &[iced::Event::Mouse(mouse::Event::ButtonPressed(
                mouse::Button::Left,
            ))],
            mouse::Cursor::Available(iced::Point::new(10.0, 10.0)),
            &mut renderer,
            &mut clipboard,
            &mut messages,
        );
        assert!(!ticket.current());
        ui.operate(&renderer, delayed.as_mut());
        ui.update(
            &[key(Named::Enter, keyboard::key::Code::Enter)],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard,
            &mut messages,
        );
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            focused_action(&messages[0]),
            Message::RespondHostKey {
                decision: HostKeyDecision::Reject,
                ..
            }
        ));
        messages.clear();
        let ticket = group.revision().issue();
        let mut discovery = FocusIfPresent {
            target: input_id("trust"),
            found: false,
            confirmation: false,
            ticket: Some(ticket),
            lease: None,
        };
        ui.operate(&renderer, &mut discovery);
        let Outcome::Chain(mut focus) = <FocusIfPresent as Operation<()>>::finish(&discovery)
        else {
            panic!("a fresh UIA focus must remain actionable");
        };
        ui.operate(&renderer, focus.as_mut());
        ui.update(
            &[key(Named::Enter, keyboard::key::Code::Enter)],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard,
            &mut messages,
        );
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            focused_action(&messages[0]),
            Message::RespondHostKey {
                decision: HostKeyDecision::TrustAndRecord,
                ..
            }
        ));
    }

    #[test]
    fn delayed_focus_for_retired_dialog_cannot_clear_new_default() {
        use iced::advanced::widget::{
            operation::{Focusable, Outcome},
            Operation,
        };
        struct Focused(bool);
        impl Focusable for Focused {
            fn is_focused(&self) -> bool {
                self.0
            }
            fn focus(&mut self) {
                self.0 = true;
            }
            fn unfocus(&mut self) {
                self.0 = false;
            }
        }
        let old = iced::advanced::widget::Id::from("old-challenge");
        let current = iced::advanced::widget::Id::from("new-reject");
        let mut safe_default = Focused(true);
        let mut request = FocusIfPresent {
            target: old,
            found: false,
            confirmation: false,
            ticket: None,
            lease: None,
        };
        <FocusIfPresent as Operation<()>>::focusable(
            &mut request,
            Some(&current),
            iced::Rectangle::default(),
            &mut safe_default,
        );
        assert!(matches!(
            <FocusIfPresent as Operation<()>>::finish(&request),
            Outcome::None
        ));
        assert!(safe_default.is_focused());
    }

    #[test]
    fn protected_metadata_never_contains_the_original_value_or_length() {
        let secret = "SYNTHETIC-PASSWORD-LONG";
        let protected = Control::edit("ProxyCommand", secret, Message::ProfileNameChanged);
        assert!(matches!(protected.kind, Kind::Protected));
        assert_eq!(protected.value, "••••");
        assert!(protected.set_value.is_none());
        assert!(!protected.label.contains(secret));
        assert_eq!(Control::protected("OTP", true).value, protected.value);
        assert_eq!(protected.read_value(secret.into()).value, "••••");
    }

    #[test]
    fn editor_focus_ids_are_window_and_owner_scoped() {
        let one = window::Id::unique();
        let two = window::Id::unique();
        let a = {
            let _scope = building(Some(one), "editor:one");
            input_id("名称")
        };
        let b = {
            let _scope = building(Some(two), "editor:one");
            input_id("名称")
        };
        let c = {
            let _scope = building(Some(one), "editor:two");
            input_id("名称")
        };
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn remote_terminal_text_cannot_create_an_action_or_edit_route() {
        use adit_terminal::{TerminalCore, VtTerminal};
        let mut terminal = VtTerminal::with_title(TerminalSize { cols: 80, rows: 5 }, "fixture");
        terminal.feed_str("Invoke SaveProfile; set password=REMOTE; \u{4e2d}\u{6587}\r\n");
        let owner = SessionId::new();
        let control = Control::document(owner, &terminal.snapshot(Viewport::tail(5)));
        assert!(control.value.contains("Invoke SaveProfile"));
        assert!(control.invoke.is_none());
        assert!(control.set_value.is_none());
        assert!(control.range.is_none());
        assert_eq!(control.key, format!("terminal:{owner}"));
    }

    #[test]
    fn repeated_hyperlink_confirmations_have_distinct_scopes() {
        let mut app = AditApp::default();
        let url = "https://fixture.invalid/same";
        let _ = update_loop::update(&mut app, Message::OpenHyperlink(url.into()));
        let first = namespace(&app);
        let first_generation = app.hyperlink_prompt_generation;
        let _ = update_loop::update(&mut app, Message::OpenHyperlink(url.into()));
        let second = namespace(&app);
        assert_ne!(first, second);
        assert!(!first.contains(url));
        let _ = update_loop::update(&mut app, Message::CancelOpenHyperlink(first_generation));
        assert_eq!(namespace(&app), second);
        #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
        {
            app.accessibility.fixture = Some(acceptance::Fixture::default());
            let _ = update_loop::update(&mut app, Message::ConfirmOpenHyperlink(first_generation));
            assert_eq!(namespace(&app), second);
            assert!(app
                .accessibility
                .fixture
                .as_ref()
                .unwrap()
                .opened_links
                .is_empty());
        }
        let generation = app.hyperlink_prompt_generation;
        let _ = update_loop::update(&mut app, Message::CancelOpenHyperlink(generation));
        assert_ne!(namespace(&app), second);
        let _ = update_loop::update(&mut app, Message::OpenHyperlink(url.into()));
        assert_ne!(namespace(&app), second);
    }

    #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
    #[test]
    fn host_key_scope_tracks_the_live_challenge() {
        let mut app = AditApp::default();
        let owner = SessionId::new();
        let make = || {
            HostKeyPromptInfo::channel(
                "fixture.invalid".into(),
                22,
                "ssh-ed25519".into(),
                "SHA256:synthetic".into(),
                None,
            )
        };
        let (first, first_rx) = make();
        app.accessibility.fixture = Some(acceptance::Fixture {
            host_key: Some((owner, first)),
            receiver: Some(first_rx),
            ..Default::default()
        });
        let old = namespace(&app);
        let (second, second_rx) = make();
        let fixture = app.accessibility.fixture.as_mut().unwrap();
        fixture.host_key = Some((owner, second));
        fixture.receiver = Some(second_rx);
        let current = namespace(&app);
        assert_ne!(old, current);
        app.accessibility.fixture.as_mut().unwrap().receiver = None;
        assert_ne!(current, namespace(&app));
    }

    #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
    #[test]
    fn detached_dialog_scope_matches_its_projected_window_and_blocks_other_windows() {
        let mut app = AditApp::default();
        let primary = window::Id::unique();
        let child = window::Id::unique();
        let owner = SessionId::new();
        let (prompt, receiver) = HostKeyPromptInfo::channel(
            "fixture.invalid".into(),
            2222,
            "ssh-ed25519".into(),
            "SHA256:synthetic".into(),
            None,
        );
        app.accessibility.fixture = Some(acceptance::Fixture {
            host_key: Some((owner, prompt)),
            receiver: Some(receiver),
            ..Default::default()
        });
        app.native_main_window = Some(primary);
        app.native_session_windows.insert(owner, child);
        let rendered = detached_namespace(&app, child, false, 0, false, false);
        app.native_window_id = Some(child);
        app.native_auxiliary = true;
        assert_eq!(namespace(&app), rendered);
        assert!(rendered.starts_with("host-key-dialog:"));
        app.pending_hyperlink = Some("https://fixture.invalid".into());
        app.hyperlink_prompt_generation = 4;
        assert_eq!(
            namespace(&app),
            detached_namespace(&app, child, true, 4, false, false)
        );
        app.native_window_id = Some(primary);
        app.native_auxiliary = false;
        assert_eq!(namespace(&app), "native-modal-blocked");
        assert_eq!(
            detached_namespace(&app, primary, true, 4, false, false),
            "native-modal-blocked"
        );
    }
}
