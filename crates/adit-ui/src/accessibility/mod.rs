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

#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
mod acceptance;
#[cfg(windows)]
mod native;
#[cfg(windows)]
mod widget;
#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
pub(super) use acceptance::run as run_acceptance;

#[derive(Default)]
pub(crate) struct State {
    pub primary: Option<window::Id>,
    pub windows: HashSet<window::Id>,
    scrolls: HashMap<(window::Id, SessionId), usize>,
    /// Only the isolated acceptance runner sets this. Shipping instances use
    /// the actual visible control actions without a fixture policy.
    pub action_filter: Option<fn(&Message) -> bool>,
}

impl State {
    pub fn opening(&mut self, window: window::Id) -> bool {
        let first = self.windows.is_empty();
        let added = self.windows.insert(window);
        if first && added {
            self.primary = Some(window);
        }
        added
    }

    pub fn closed(&mut self, window: window::Id) {
        self.windows.remove(&window);
        self.scrolls.retain(|(owner, _), _| *owner != window);
        if self.primary == Some(window) {
            self.primary = None;
        }
        #[cfg(windows)]
        native::unregister(window);
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Button,
    Item,
    Edit,
    Protected,
    Document,
    Range,
}

// No Debug implementation: even benign user-editable text can contain secrets.
#[derive(Clone)]
pub(crate) struct Control {
    key: String,
    label: String,
    kind: Kind,
    value: String,
    invoke: Option<Message>,
    set_value: Option<fn(String) -> Message>,
    range: Option<(Option<window::Id>, SessionId, usize, usize)>,
    focus: Option<iced::advanced::widget::Id>,
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
        }
    }

    pub fn item(key: impl Into<String>, label: String, message: Message) -> Self {
        Self {
            kind: Kind::Item,
            ..Self::button(key, label, message)
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

pub(crate) fn modal<'a>(child: Element<'a, Message>) -> Element<'a, Message> {
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
        native::drain(&app.accessibility.windows, &namespace(app))
            .into_iter()
            .filter(|(_, _, message)| {
                app.accessibility
                    .action_filter
                    .is_none_or(|allow| allow(message))
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        let _ = app;
        Vec::new()
    }
}

pub(crate) fn action_current(app: &AditApp, window: window::Id, scope: &str) -> bool {
    app.accessibility.primary == Some(window)
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
    app.accessibility
        .primary
        .and_then(|window| app.accessibility.scrolls.get(&(window, owner)).copied())
        .unwrap_or(0)
}

pub(crate) fn scroll_lines(app: &mut AditApp, window: window::Id, owner: SessionId, lines: i32) {
    let current = if app.accessibility.primary == Some(window)
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

pub(crate) fn namespace(app: &AditApp) -> String {
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
    if app.manager.sftp_is_open() {
        return "sftp".into();
    }
    if app.pending_hyperlink.is_some() {
        return "hyperlink".into();
    }
    if let Some((_, prompt)) = &app.auth_prompt {
        return format!("challenge:{}", prompt.id);
    }
    if app.manager.pending_host_key().is_some() {
        return "host-key-dialog".into();
    }
    if app.connection_dialog.is_some() {
        return "connection-dialog".into();
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
