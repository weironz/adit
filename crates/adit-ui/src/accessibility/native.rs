//! Official AccessKit UIA adapter with a chain-safe, per-HWND subclass.
//! Installation runs through iced::window::run on the owning window thread.
use super::*;
use accesskit::{
    Action, ActionData, ActionRequest, Node, NodeId, Role, TreeId, TreeInfo, TreeUpdate,
};
use accesskit_windows::{Adapter, QueuedEvents};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::{
        HiDpi::GetDpiForWindow,
        Input::KeyboardAndMouse::GetFocus,
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId, IsWindow, SetForegroundWindow,
            WM_GETOBJECT, WM_KILLFOCUS, WM_NCDESTROY, WM_SETFOCUS,
        },
    },
};

const ROOT: NodeId = NodeId(1);
const SUBCLASS: usize = 0x4144_4954;

#[derive(Debug, Default)]
pub(super) struct ForegroundState {
    generation: u64,
    windows: HashSet<isize>,
}

fn foreground_state() -> Arc<Mutex<ForegroundState>> {
    static STATE: OnceLock<Arc<Mutex<ForegroundState>>> = OnceLock::new();
    Arc::clone(STATE.get_or_init(|| Arc::new(Mutex::new(ForegroundState::default()))))
}

fn capture_foreground(
    target: isize,
    scope_epoch: Arc<AtomicU64>,
    scope_generation: u64,
) -> Option<WindowFocusLease> {
    let state = foreground_state();
    let foreground = unsafe { GetForegroundWindow() }.0 as isize;
    let generation = {
        let guard = state.lock().ok()?;
        if !guard.windows.contains(&foreground) || !guard.windows.contains(&target) {
            return None;
        }
        guard.generation
    };
    Some(WindowFocusLease {
        state,
        generation,
        foreground,
        target,
        scope_epoch,
        scope_generation,
    })
}

fn lease_current_for(lease: &WindowFocusLease, foreground: isize) -> bool {
    lease.state.lock().is_ok_and(|state| {
        lease.scope_epoch.load(Ordering::SeqCst) == lease.scope_generation
            && state.generation == lease.generation
            && state.windows.contains(&lease.target)
            && state.windows.contains(&foreground)
            && foreground == lease.foreground
    })
}

pub(super) fn foreground_lease_current(lease: &WindowFocusLease) -> bool {
    lease_current_for(lease, unsafe { GetForegroundWindow() }.0 as isize)
}

pub(super) fn activate_for_focus(
    lease: &WindowFocusLease,
    ticket: Option<&dialog_focus::FocusTicket>,
) -> Option<WindowFocusLease> {
    defer_focus_events(|| {
        let activate = || {
            if !foreground_lease_current(lease) {
                return None;
            }
            if lease.foreground != lease.target {
                let hwnd = HWND(lease.target as *mut std::ffi::c_void);
                if !unsafe { SetForegroundWindow(hwnd) }.as_bool()
                    || unsafe { GetForegroundWindow() } != hwnd
                {
                    return None;
                }
            }
            capture_foreground(
                lease.target,
                Arc::clone(&lease.scope_epoch),
                lease.scope_generation,
            )
            .filter(|fresh| fresh.foreground == fresh.target && fresh.current())
        };
        if let Some(ticket) = ticket {
            ticket.while_pending(activate).flatten()
        } else {
            activate()
        }
    })
}

// SetForegroundWindow synchronously sends WM_SETFOCUS/WM_KILLFOCUS. Defer
// AccessKit notifications until the ticket lock is released: UIA listeners
// can re-enter the provider and issue another focus request from those events.
fn defer_focus_events<T>(action: impl FnOnce() -> T) -> T {
    let nested = DEFER_FOCUS_EVENTS.with(|defer| defer.replace(true));
    let result = action();
    if !nested {
        DEFER_FOCUS_EVENTS.with(|defer| defer.set(false));
        let events = DEFERRED_FOCUS_EVENTS.with(|queue| std::mem::take(&mut *queue.borrow_mut()));
        for event in events {
            event.raise();
        }
    }
    result
}

fn raise_focus_events(events: QueuedEvents) {
    if DEFER_FOCUS_EVENTS.with(Cell::get) {
        DEFERRED_FOCUS_EVENTS.with(|queue| queue.borrow_mut().push(events));
    } else {
        events.raise();
    }
}

#[derive(Clone)]
struct Route {
    invoke: Option<Message>,
    set_value: Option<fn(String) -> Message>,
    focus: Option<iced::advanced::widget::Id>,
    focus_revision: Option<dialog_focus::FocusRevision>,
    focus_index: Option<usize>,
    range: Option<(window::Id, SessionId)>,
}

struct Shared {
    tree: Option<TreeUpdate>,
    routes: HashMap<NodeId, Route>,
    namespace: String,
    revoked: bool,
    scope_epoch: Arc<AtomicU64>,
    #[cfg(feature = "native-accessibility-acceptance")]
    focus_enqueued: std::sync::atomic::AtomicU64,
    #[cfg(feature = "native-accessibility-acceptance")]
    stale_focus_rejected: std::sync::atomic::AtomicU64,
}

struct Activate(Arc<Mutex<Shared>>);
impl accesskit::ActivationHandler for Activate {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        self.0.lock().ok()?.tree.clone()
    }
}

// Never Debug or log payloads. The queue only receives validated, non-protected
// edits, has a finite capacity, and every request is checked again on dispatch.
struct Request {
    node: NodeId,
    namespace: String,
    action: Action,
    value: Option<String>,
    focus_ticket: Option<dialog_focus::FocusTicket>,
    activation: Option<dialog_focus::FocusActivation>,
    window_lease: Option<WindowFocusLease>,
}

struct Actions {
    state: Arc<Mutex<Shared>>,
    tx: mpsc::SyncSender<Request>,
    hwnd: isize,
    #[cfg(test)]
    allow_unregistered: bool,
}

impl accesskit::ActionHandler for Actions {
    fn do_action(&mut self, request: ActionRequest) {
        if request.target_tree != TreeId::ROOT {
            return;
        }
        let (route, namespace, scope_epoch, scope_generation) = {
            let Ok(state) = self.state.lock() else { return };
            if state.revoked {
                return;
            }
            let Some(route) = state.routes.get(&request.target_node) else {
                return;
            };
            (
                route.clone(),
                state.namespace.clone(),
                Arc::clone(&state.scope_epoch),
                state.scope_epoch.load(Ordering::SeqCst),
            )
        };
        let mut out = Request {
            node: request.target_node,
            namespace,
            action: request.action,
            value: None,
            focus_ticket: None,
            activation: None,
            window_lease: None,
        };
        match (request.action, request.data) {
            (Action::Focus, _) if route.focus.is_some() => {
                #[cfg(test)]
                let allow_unregistered = self.allow_unregistered;
                #[cfg(not(test))]
                let allow_unregistered = false;
                out.window_lease =
                    capture_foreground(self.hwnd, Arc::clone(&scope_epoch), scope_generation);
                if out.window_lease.is_none() && !allow_unregistered {
                    return;
                }
                if let Some(revision) = &route.focus_revision {
                    let queued = revision.enqueue(route.focus_index, |ticket| {
                        out.focus_ticket = Some(ticket);
                        self.tx.try_send(out).is_ok()
                    });
                    #[cfg(feature = "native-accessibility-acceptance")]
                    if queued {
                        if let Ok(state) = self.state.lock() {
                            state
                                .focus_enqueued
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    let _ = queued;
                    return;
                }
            }
            (Action::Click, _) if route.invoke.is_some() => {
                if let Some(revision) = &route.focus_revision {
                    #[cfg(test)]
                    let allow_unregistered = self.allow_unregistered;
                    #[cfg(not(test))]
                    let allow_unregistered = false;
                    out.window_lease =
                        capture_foreground(self.hwnd, Arc::clone(&scope_epoch), scope_generation)
                            .filter(|lease| lease.foreground == lease.target);
                    if out.window_lease.is_none() && !allow_unregistered {
                        return;
                    }
                    let _ = revision.enqueue_activation(route.focus_index, |activation| {
                        out.activation = Some(activation);
                        self.tx.try_send(out).is_ok()
                    });
                    return;
                }
            }
            (Action::SetValue, Some(ActionData::Value(value)))
                if (route.set_value.is_some() || route.range.is_some()) && value.len() <= 8192 =>
            {
                out.value = Some(value.into());
            }
            (Action::SetValue, Some(ActionData::NumericValue(value))) if route.range.is_some() => {
                out.value = Some(value.to_string());
            }
            _ => return,
        }
        #[cfg(feature = "native-accessibility-acceptance")]
        let focus_request = out.action == Action::Focus;
        let queued = self.tx.try_send(out);
        #[cfg(feature = "native-accessibility-acceptance")]
        if focus_request && queued.is_ok() {
            if let Ok(state) = self.state.lock() {
                state
                    .focus_enqueued
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let _ = queued;
    }
}

struct Window {
    hwnd: HWND,
    alive: Cell<bool>,
    adapter: RefCell<Adapter>,
    shared: Arc<Mutex<Shared>>,
    rx: mpsc::Receiver<Request>,
    ids: NodeIds,
    last: Option<TreeUpdate>,
}

struct NodeIds {
    keys: HashMap<String, NodeId>,
    next: u64,
}

impl Default for NodeIds {
    fn default() -> Self {
        Self {
            keys: HashMap::new(),
            next: 2,
        }
    }
}

impl NodeIds {
    fn get(&mut self, key: String) -> NodeId {
        *self.keys.entry(key).or_insert_with(|| {
            let id = NodeId(self.next);
            self.next = self.next.checked_add(1).expect("UIA node IDs exhausted");
            id
        })
    }

    fn retain(&mut self, live: &HashSet<NodeId>) {
        self.keys.retain(|_, id| live.contains(id));
    }
}

thread_local! {
    static WINDOWS: RefCell<HashMap<window::Id, Box<Window>>> = RefCell::new(HashMap::new());
    static DEFER_FOCUS_EVENTS: Cell<bool> = const { Cell::new(false) };
    static DEFERRED_FOCUS_EVENTS: RefCell<Vec<QueuedEvents>> = const { RefCell::new(Vec::new()) };
    #[cfg(feature = "native-accessibility-acceptance")]
    static HELD_ACTIONS: RefCell<HashSet<window::Id>> = RefCell::new(HashSet::new());
}

#[cfg(feature = "native-accessibility-acceptance")]
pub(super) fn hold_actions(id: window::Id, hold: bool) {
    HELD_ACTIONS.with(|held| {
        if hold {
            held.borrow_mut().insert(id);
        } else {
            held.borrow_mut().remove(&id);
        }
    });
}

unsafe extern "system" fn subclass(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    data: usize,
) -> LRESULT {
    // The Box remains pinned in WINDOWS until the subclass has been removed.
    let window = unsafe { &*(data as *const Window) };
    match message {
        WM_GETOBJECT => {
            let mut activate = Activate(Arc::clone(&window.shared));
            let result =
                window
                    .adapter
                    .borrow_mut()
                    .handle_wm_getobject(wparam, lparam, &mut activate);
            if let Some(result) = result {
                return result.into();
            }
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            if let Ok(mut state) = foreground_state().lock() {
                state.generation = state.generation.wrapping_add(1);
            }
            let events = window
                .adapter
                .borrow_mut()
                .update_window_focus_state(message == WM_SETFOCUS);
            if let Some(events) = events {
                raise_focus_events(events);
            }
        }
        WM_NCDESTROY => {
            window.alive.set(false);
            if let Ok(mut state) = foreground_state().lock() {
                state.windows.remove(&(hwnd.0 as isize));
                state.generation = state.generation.wrapping_add(1);
            }
            if let Ok(mut state) = window.shared.lock() {
                state.revoked = true;
                state.scope_epoch.fetch_add(1, Ordering::SeqCst);
                state.routes.clear();
                state.tree = None;
            }
        }
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

pub(super) fn register(id: window::Id, raw: isize) -> bool {
    let hwnd = HWND(raw as *mut std::ffi::c_void);
    if unsafe { GetWindowThreadProcessId(hwnd, None) != GetCurrentThreadId() } {
        return false;
    }
    WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        windows.retain(|_, window| window.alive.get());
        if let Some(existing) = windows.get(&id) {
            return existing.hwnd == hwnd;
        }
        if windows.values().any(|window| window.hwnd == hwnd) {
            return false;
        }
        let shared = Arc::new(Mutex::new(Shared {
            tree: None,
            routes: HashMap::new(),
            namespace: String::new(),
            revoked: false,
            scope_epoch: Arc::new(AtomicU64::new(0)),
            #[cfg(feature = "native-accessibility-acceptance")]
            focus_enqueued: std::sync::atomic::AtomicU64::new(0),
            #[cfg(feature = "native-accessibility-acceptance")]
            stale_focus_rejected: std::sync::atomic::AtomicU64::new(0),
        }));
        let (tx, rx) = mpsc::sync_channel(64);
        let window = Box::new(Window {
            hwnd,
            alive: Cell::new(true),
            adapter: RefCell::new(Adapter::new(
                hwnd,
                unsafe { GetFocus() == hwnd },
                Actions {
                    state: Arc::clone(&shared),
                    tx,
                    hwnd: raw,
                    #[cfg(test)]
                    allow_unregistered: false,
                },
            )),
            shared,
            rx,
            ids: NodeIds::default(),
            last: None,
        });
        let pointer = (&*window as *const Window) as usize;
        if !unsafe { SetWindowSubclass(hwnd, Some(subclass), SUBCLASS, pointer) }.as_bool() {
            return false;
        }
        if let Ok(mut state) = foreground_state().lock() {
            state.windows.insert(raw);
            state.generation = state.generation.wrapping_add(1);
        }
        windows.insert(id, window);
        true
    })
}

pub(super) fn unregister(id: window::Id) {
    let removed = WINDOWS.with(|windows| windows.borrow_mut().remove(&id));
    if let Some(window) = removed {
        if let Ok(mut state) = foreground_state().lock() {
            state.windows.remove(&(window.hwnd.0 as isize));
            state.generation = state.generation.wrapping_add(1);
        }
        if let Ok(mut state) = window.shared.lock() {
            state.revoked = true;
            state.scope_epoch.fetch_add(1, Ordering::SeqCst);
            state.routes.clear();
            state.tree = None;
        }
        let detached = !window.alive.get()
            || !unsafe { IsWindow(Some(window.hwnd)) }.as_bool()
            || unsafe { RemoveWindowSubclass(window.hwnd, Some(subclass), SUBCLASS) }.as_bool();
        // Remove the document and editor data even if a UIA client retains a
        // provider reference after our window disappears.
        let mut root = Node::new(Role::Window);
        root.set_label("Adit");
        root.set_disabled();
        let events = window.adapter.borrow_mut().update_if_active(|| TreeUpdate {
            nodes: vec![(ROOT, root)],
            tree: Some(TreeInfo::new(ROOT)),
            tree_id: TreeId::ROOT,
            focus: ROOT,
        });
        if let Some(events) = events {
            events.raise();
        }
        if !detached {
            // Preserve the callback's allocation if Windows could not detach
            // it. It is revoked and empty; WM_NCDESTROY makes it reclaimable.
            WINDOWS.with(|windows| windows.borrow_mut().insert(id, window));
        }
    }
}

pub(super) fn node_id(window: window::Id, key: &str) -> NodeId {
    WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        let Some(window) = windows.get_mut(&window) else {
            return ROOT;
        };
        window.ids.get(key.to_owned())
    })
}

pub(super) struct Rendered {
    pub id: NodeId,
    pub parent: NodeId,
    pub layer: usize,
    pub control: Control,
    pub bounds: iced::Rectangle,
    pub focused: bool,
}

pub(super) fn publish(
    id: window::Id,
    namespace: String,
    rendered: Vec<Rendered>,
    layer: usize,
    bounds: iced::Rectangle,
) {
    let events = WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        let window = windows.get_mut(&id)?;
        if !window.alive.get() {
            return None;
        }
        let scale = f64::from(unsafe { GetDpiForWindow(window.hwnd) }) / 96.0;
        let rect = |r: iced::Rectangle| {
            accesskit::Rect::new(
                f64::from(r.x) * scale,
                f64::from(r.y) * scale,
                f64::from(r.x + r.width) * scale,
                f64::from(r.y + r.height) * scale,
            )
        };
        // Keep only IDs present in this frame. Old providers must never be
        // retargeted, so retired keys are discarded but numeric IDs are not reused.
        let mut live_ids: HashSet<_> = rendered.iter().map(|r| r.id).collect();
        let visible: Vec<_> = rendered.into_iter().filter(|r| r.layer == layer).collect();
        let valid: HashSet<_> = visible.iter().map(|r| r.id).collect();
        let mut nodes = HashMap::new();
        let mut children: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        let mut routes = HashMap::new();
        let mut focused = ROOT;
        for item in visible {
            let control = item.control;
            let role = match control.kind {
                Kind::Button => Role::Button,
                Kind::Dialog => Role::Dialog,
                Kind::Label => Role::Label,
                Kind::Item => Role::ListItem,
                Kind::Edit => Role::TextInput,
                Kind::Protected => Role::PasswordInput,
                Kind::Document => Role::Terminal,
                Kind::Range => Role::Slider,
            };
            let mut node = Node::new(role);
            if matches!(control.kind, Kind::Label) {
                node.set_value(control.label);
            } else {
                node.set_label(control.label);
            }
            if matches!(control.kind, Kind::Dialog) {
                node.set_modal();
            }
            node.set_author_id(format!("{namespace}/{}", control.key));
            node.set_bounds(rect(item.bounds));
            if control.focus.is_some() {
                node.add_action(Action::Focus);
            }
            if item.focused {
                focused = item.id;
            }
            if matches!(
                control.kind,
                Kind::Item | Kind::Edit | Kind::Protected | Kind::Document
            ) {
                node.set_value(if matches!(control.kind, Kind::Protected) {
                    if control.value.is_empty() {
                        String::new()
                    } else {
                        "••••".into()
                    }
                } else {
                    control.value.clone()
                });
            }
            if matches!(control.kind, Kind::Item | Kind::Protected) {
                node.set_read_only();
            }
            if matches!(control.kind, Kind::Button) && control.invoke.is_none() {
                node.set_disabled();
            }
            if control.invoke.is_some() {
                node.add_action(Action::Click);
            }
            if control.set_value.is_some()
                || control
                    .range
                    .is_some_and(|(window, _, _, _)| window.is_some())
            {
                node.add_action(Action::SetValue);
            }
            if let Some((_, _, value, max)) = control.range {
                node.set_numeric_value(value as f64);
                node.set_min_numeric_value(0.0);
                node.set_max_numeric_value(max as f64);
                node.set_numeric_value_step(1.0);
            }
            if matches!(control.kind, Kind::Document) {
                let key = format!("{namespace}/{}/text", control.key);
                let text_id = window.ids.get(key);
                live_ids.insert(text_id);
                let mut run = Node::new(Role::TextRun);
                run.set_bounds(rect(item.bounds));
                run.set_character_lengths(
                    control
                        .value
                        .chars()
                        .map(|c| c.len_utf8() as u8)
                        .collect::<Vec<_>>(),
                );
                run.set_value(control.value);
                node.set_read_only();
                children.entry(item.id).or_default().push(text_id);
                nodes.insert(text_id, run);
            }
            routes.insert(
                item.id,
                Route {
                    invoke: control.invoke,
                    set_value: control.set_value,
                    focus: control.focus,
                    focus_revision: control.focus_revision,
                    focus_index: control.focus_index,
                    range: control
                        .range
                        .and_then(|(window, owner, _, _)| window.map(|window| (window, owner))),
                },
            );
            let parent = if valid.contains(&item.parent) {
                item.parent
            } else {
                ROOT
            };
            children.entry(parent).or_default().push(item.id);
            nodes.insert(item.id, node);
        }
        let mut root = Node::new(Role::Window);
        root.set_label("Adit");
        root.set_bounds(rect(bounds));
        nodes.insert(ROOT, root);
        for (parent, children) in children {
            if let Some(node) = nodes.get_mut(&parent) {
                node.set_children(children);
            }
        }
        let mut nodes: Vec<_> = nodes.into_iter().collect();
        nodes.sort_by_key(|(id, _)| id.0);
        let tree = TreeUpdate {
            nodes,
            tree: Some(TreeInfo::new(ROOT)),
            tree_id: TreeId::ROOT,
            focus: focused,
        };
        window.ids.retain(&live_ids);
        if let Ok(mut state) = window.shared.lock() {
            retire_shared_scope(&mut state, &namespace);
            state.routes = routes;
            state.tree = Some(tree.clone());
        }
        if window.last.as_ref() == Some(&tree) {
            return None;
        }
        window.last = Some(tree.clone());
        let events = window.adapter.borrow_mut().update_if_active(|| tree);
        events
    });
    // UIA event raising may re-enter the HWND. No registry/adapter borrow is
    // held here, so WM_GETOBJECT can synchronously obtain the current tree.
    if let Some(events) = events {
        events.raise();
    }
}

pub(super) fn live(id: window::Id) -> bool {
    WINDOWS.with(|windows| {
        windows
            .borrow()
            .get(&id)
            .is_some_and(|window| window.alive.get())
    })
}

pub(super) fn retire_scope(id: window::Id, namespace: &str) {
    WINDOWS.with(|windows| {
        let windows = windows.borrow();
        let Some(window) = windows.get(&id) else {
            return;
        };
        let Ok(mut state) = window.shared.lock() else {
            return;
        };
        retire_shared_scope(&mut state, namespace);
    });
}

fn retire_shared_scope(state: &mut Shared, namespace: &str) {
    if state.namespace != namespace {
        state.scope_epoch.fetch_add(1, Ordering::SeqCst);
        for revision in state
            .routes
            .values()
            .filter_map(|route| route.focus_revision.as_ref())
        {
            revision.retire();
        }
        state.routes.clear();
        state.namespace = namespace.to_owned();
    }
}

#[cfg(feature = "native-accessibility-acceptance")]
pub(super) fn focus_statistics(id: window::Id) -> (u64, u64) {
    WINDOWS.with(|windows| {
        let windows = windows.borrow();
        let Some(window) = windows.get(&id) else {
            return (0, 0);
        };
        let Ok(state) = window.shared.lock() else {
            return (0, 0);
        };
        use std::sync::atomic::Ordering::Relaxed;
        (
            state.focus_enqueued.load(Relaxed),
            state.stale_focus_rejected.load(Relaxed),
        )
    })
}

fn current(state: &Shared, request: &Request, namespace: &str) -> bool {
    !state.revoked
        && request.namespace == namespace
        && state.namespace == namespace
        && request
            .window_lease
            .as_ref()
            .is_none_or(WindowFocusLease::current)
        && request.focus_ticket.as_ref().is_none_or(|ticket| {
            ticket.current()
                && state.routes.get(&request.node).is_some_and(|route| {
                    route
                        .focus_revision
                        .as_ref()
                        .is_some_and(|revision| ticket.belongs_to(revision))
                })
        })
        && request.activation.as_ref().is_none_or(|activation| {
            activation.current()
                && state.routes.get(&request.node).is_some_and(|route| {
                    route
                        .focus_revision
                        .as_ref()
                        .is_some_and(|revision| activation.belongs_to(revision))
                })
        })
}

pub(super) fn drain(
    registered: &HashSet<window::Id>,
    current_window: Option<window::Id>,
    namespace: &str,
) -> Vec<(window::Id, String, Message)> {
    WINDOWS.with(|windows| {
        let windows = windows.borrow();
        let mut messages = Vec::new();
        for (id, window) in windows
            .iter()
            .filter(|(id, _)| Some(**id) == current_window && registered.contains(id))
        {
            #[cfg(feature = "native-accessibility-acceptance")]
            if HELD_ACTIONS.with(|held| held.borrow().contains(id)) {
                continue;
            }
            for _ in 0..32 {
                let Ok(request) = window.rx.try_recv() else {
                    break;
                };
                let Ok(state) = window.shared.lock() else {
                    continue;
                };
                if !current(&state, &request, namespace) {
                    if let Some(ticket) = &request.focus_ticket {
                        ticket.cancel();
                    }
                    #[cfg(feature = "native-accessibility-acceptance")]
                    if request.action == Action::Focus {
                        state
                            .stale_focus_rejected
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    continue;
                }
                let Some(route) = state.routes.get(&request.node) else {
                    continue;
                };
                let message = match request.action {
                    Action::Focus => route
                        .focus
                        .clone()
                        .map(|focus| Message::AccessibilityFocus {
                            window: *id,
                            id: focus,
                            ticket: request.focus_ticket,
                            lease: request.window_lease,
                        }),
                    Action::Click => route.invoke.clone().map(|action| {
                        if let Some(activation) = request.activation {
                            Message::DialogFocusActivate {
                                activation: Some(activation),
                                action: Box::new(action),
                                lease: request.window_lease,
                            }
                        } else {
                            action
                        }
                    }),
                    Action::SetValue if route.set_value.is_some() => {
                        request.value.map(|value| (route.set_value.unwrap())(value))
                    }
                    Action::SetValue if route.range.is_some() => request
                        .value
                        .and_then(|value| value.parse::<f64>().ok())
                        .and_then(|value| {
                            let (window, owner) = route.range?;
                            value.is_finite().then(|| Message::AccessibilityScroll {
                                window,
                                owner,
                                offset: value.max(0.0).round() as usize,
                            })
                        }),
                    _ => None,
                };
                if let Some(message) = message {
                    messages.push((*id, request.namespace, message));
                }
            }
        }
        messages
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_generation_ids_are_pruned_without_reusing_a_provider_id() {
        let mut ids = NodeIds::default();
        let persistent = ids.get("main/navigation".into());
        let mut retired = HashSet::new();
        for generation in 0..10_000 {
            let key = format!("challenge:{generation}/reject");
            let current = ids.get(key.clone());
            assert!(!retired.contains(&current));
            assert_eq!(ids.get(key), current);
            ids.retain(&HashSet::from([persistent, current]));
            assert_eq!(ids.keys.len(), 2);
            retired.insert(current);
        }
        let new = ids.get("challenge:replacement/reject".into());
        assert!(!retired.contains(&new));
    }

    fn handler(route: Route) -> (Actions, mpsc::Receiver<Request>) {
        let (tx, rx) = mpsc::sync_channel(64);
        let shared = Shared {
            tree: None,
            routes: HashMap::from([(NodeId(2), route)]),
            namespace: "editor:fixture".into(),
            revoked: false,
            scope_epoch: Arc::new(AtomicU64::new(0)),
            #[cfg(feature = "native-accessibility-acceptance")]
            focus_enqueued: std::sync::atomic::AtomicU64::new(0),
            #[cfg(feature = "native-accessibility-acceptance")]
            stale_focus_rejected: std::sync::atomic::AtomicU64::new(0),
        };
        (
            Actions {
                state: Arc::new(Mutex::new(shared)),
                tx,
                hwnd: 0,
                allow_unregistered: true,
            },
            rx,
        )
    }

    fn request(action: Action, data: Option<ActionData>) -> ActionRequest {
        ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node: NodeId(2),
            data,
        }
    }

    #[test]
    fn protected_actions_accept_only_explicit_focus_and_never_queue_values() {
        use accesskit::ActionHandler;
        let (mut handler, rx) = handler(Route {
            invoke: None,
            set_value: None,
            range: None,
            focus: Some(iced::advanced::widget::Id::new("fixture-password")),
            focus_revision: None,
            focus_index: None,
        });
        handler.do_action(request(
            Action::SetValue,
            Some(ActionData::Value("SYNTHETIC-SECRET".into())),
        ));
        assert!(rx.try_recv().is_err());
        handler.do_action(request(Action::Focus, None));
        let focus = rx.try_recv().unwrap();
        assert_eq!(focus.action, Action::Focus);
        assert!(focus.value.is_none());
    }

    #[test]
    fn newer_focus_selection_invalidates_queued_uia_focus_in_the_same_dialog() {
        use accesskit::ActionHandler;
        let revision = dialog_focus::FocusGroup::new().revision();
        let (mut handler, rx) = handler(Route {
            invoke: None,
            set_value: None,
            range: None,
            focus: Some(iced::advanced::widget::Id::new("trust")),
            focus_revision: Some(revision.clone()),
            focus_index: Some(3),
        });
        handler.do_action(request(Action::Focus, None));
        let old = rx.try_recv().unwrap();
        assert!(current(
            &handler.state.lock().unwrap(),
            &old,
            "editor:fixture"
        ));
        let newer = revision.issue();
        assert!(newer.current());
        assert!(!current(
            &handler.state.lock().unwrap(),
            &old,
            "editor:fixture"
        ));
        handler.do_action(request(Action::Focus, None));
        let queued = rx.try_recv().unwrap();
        assert!(current(
            &handler.state.lock().unwrap(),
            &queued,
            "editor:fixture"
        ));
        let other_window = dialog_focus::FocusGroup::new().revision();
        handler
            .state
            .lock()
            .unwrap()
            .routes
            .get_mut(&NodeId(2))
            .unwrap()
            .focus_revision = Some(other_window);
        assert!(!current(
            &handler.state.lock().unwrap(),
            &queued,
            "editor:fixture"
        ));
    }

    #[test]
    fn full_queue_does_not_invalidate_an_accepted_focus_request() {
        use accesskit::ActionHandler;
        let revision = dialog_focus::FocusGroup::new().revision();
        let (mut handler, rx) = handler(Route {
            invoke: Some(Message::CloseProfileEditor),
            set_value: None,
            range: None,
            focus: Some(iced::advanced::widget::Id::new("reject")),
            focus_revision: Some(revision),
            focus_index: Some(0),
        });
        for _ in 0..63 {
            handler.do_action(request(Action::Click, None));
        }
        handler.do_action(request(Action::Focus, None));
        handler.do_action(request(Action::Focus, None));
        handler.do_action(request(Action::Click, None));
        let queued: Vec<_> = rx.try_iter().collect();
        assert_eq!(queued.len(), 64);
        let accepted = queued.last().unwrap();
        assert!(accepted.focus_ticket.as_ref().unwrap().current());
        assert!(current(
            &handler.state.lock().unwrap(),
            accepted,
            "editor:fixture"
        ));
    }

    #[test]
    fn uia_invoke_cannot_overtake_an_accepted_focus_request() {
        use accesskit::ActionHandler;
        let revision = dialog_focus::FocusGroup::new().revision();
        let (mut handler, rx) = handler(Route {
            invoke: Some(Message::ClearError),
            set_value: None,
            range: None,
            focus: Some(iced::advanced::widget::Id::new("trust")),
            focus_revision: Some(revision),
            focus_index: Some(3),
        });
        handler.do_action(request(Action::Click, None));
        let invoke = rx.try_recv().unwrap();
        assert!(invoke.activation.as_ref().unwrap().current());
        handler
            .state
            .lock()
            .unwrap()
            .routes
            .get_mut(&NodeId(2))
            .unwrap()
            .focus_index = Some(0);
        handler.do_action(request(Action::Focus, None));
        let focus = rx.try_recv().unwrap();
        assert!(focus.focus_ticket.as_ref().unwrap().current());
        assert!(!invoke.activation.as_ref().unwrap().current());
        handler
            .state
            .lock()
            .unwrap()
            .routes
            .get_mut(&NodeId(2))
            .unwrap()
            .focus_index = Some(3);
        handler.do_action(request(Action::Click, None));
        assert!(rx.try_recv().is_err(), "pending focus accepted an invoke");
        handler
            .state
            .lock()
            .unwrap()
            .routes
            .get_mut(&NodeId(2))
            .unwrap()
            .focus_index = Some(0);
        handler.do_action(request(Action::Click, None));
        let same_target = rx.try_recv().unwrap();
        let activation = same_target.activation.as_ref().unwrap();
        assert!(activation.current());
        assert!(!focus.focus_ticket.as_ref().unwrap().current());
        assert!(activation.commit(|| false, || ()).is_none());
        assert!(activation.current());
        assert!(activation.commit(|| true, || ()).is_some());
        assert!(!focus.focus_ticket.as_ref().unwrap().current());
    }

    #[test]
    fn later_uia_rejection_supersedes_an_earlier_trust_invoke() {
        use accesskit::ActionHandler;
        let revision = dialog_focus::FocusGroup::new().revision();
        let (mut handler, rx) = handler(Route {
            invoke: Some(Message::ClearError),
            set_value: None,
            range: None,
            focus: Some(iced::advanced::widget::Id::new("trust")),
            focus_revision: Some(revision),
            focus_index: Some(3),
        });
        handler.do_action(request(Action::Click, None));
        let trust = rx.try_recv().unwrap();
        assert!(trust.activation.as_ref().unwrap().current());
        handler
            .state
            .lock()
            .unwrap()
            .routes
            .get_mut(&NodeId(2))
            .unwrap()
            .focus_index = Some(0);
        handler.do_action(request(Action::Click, None));
        let rejection = rx.try_recv().unwrap();
        assert!(!trust.activation.as_ref().unwrap().current());
        assert!(rejection.activation.as_ref().unwrap().current());
    }

    #[test]
    fn window_switch_revokes_the_original_foreground_lease_even_after_switching_back() {
        let state = Arc::new(Mutex::new(ForegroundState {
            generation: 3,
            windows: HashSet::from([1, 2]),
        }));
        let lease = WindowFocusLease {
            state: Arc::clone(&state),
            generation: 3,
            foreground: 1,
            target: 2,
            scope_epoch: Arc::new(AtomicU64::new(0)),
            scope_generation: 0,
        };
        assert!(lease_current_for(&lease, 1));
        assert!(!lease_current_for(&lease, 2));
        state.lock().unwrap().generation += 2;
        assert!(!lease_current_for(&lease, 1));
    }

    #[test]
    fn closing_the_scope_revokes_a_plain_window_focus_lease() {
        let scope_epoch = Arc::new(AtomicU64::new(4));
        let lease = WindowFocusLease {
            state: Arc::new(Mutex::new(ForegroundState {
                generation: 3,
                windows: HashSet::from([1, 2]),
            })),
            generation: 3,
            foreground: 1,
            target: 2,
            scope_epoch: Arc::clone(&scope_epoch),
            scope_generation: 4,
        };
        assert!(lease_current_for(&lease, 1));
        scope_epoch.fetch_add(1, Ordering::SeqCst);
        assert!(!lease_current_for(&lease, 1));
    }

    #[test]
    fn writable_range_actions_accept_numeric_set_value() {
        use accesskit::ActionHandler;
        let (mut handler, rx) = handler(Route {
            invoke: None,
            set_value: None,
            focus: None,
            focus_revision: None,
            focus_index: None,
            range: Some((window::Id::unique(), SessionId::new())),
        });
        handler.do_action(request(
            Action::SetValue,
            Some(ActionData::NumericValue(42.0)),
        ));
        let request = rx.try_recv().unwrap();
        assert_eq!(request.action, Action::SetValue);
        assert_eq!(request.value.as_deref(), Some("42"));
    }

    #[test]
    fn queues_are_bounded_and_old_modal_or_revoked_actions_are_rejected() {
        use accesskit::ActionHandler;
        let (mut handler, rx) = handler(Route {
            invoke: Some(Message::CloseProfileEditor),
            set_value: None,
            focus: None,
            focus_revision: None,
            focus_index: None,
            range: None,
        });
        for _ in 0..100 {
            handler.do_action(request(Action::Click, None));
        }
        let queued: Vec<_> = rx.try_iter().collect();
        assert_eq!(queued.len(), 64);
        let mut state = handler.state.lock().unwrap();
        assert!(current(&state, &queued[0], "editor:fixture"));
        state.namespace = "main".into();
        assert!(!current(&state, &queued[0], "main"));
        state.namespace = "editor:fixture".into();
        state.revoked = true;
        assert!(!current(&state, &queued[0], "editor:fixture"));
    }

    #[test]
    fn real_hwnd_leases_register_and_revoke_independently() {
        use windows::{
            core::{w, HSTRING},
            Win32::{
                Foundation::HINSTANCE,
                System::LibraryLoader::GetModuleHandleW,
                UI::WindowsAndMessaging::{
                    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW,
                    UnregisterClassW, WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPEDWINDOW,
                },
            },
        };
        unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, w, l) }
        }
        unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None).unwrap().0);
            let name = HSTRING::from(format!("Adit-SYNTHETIC-UIA-lease-{}", GetCurrentThreadId()));
            let class = WNDCLASSW {
                hInstance: instance,
                lpfnWndProc: Some(proc),
                lpszClassName: windows::core::PCWSTR(name.as_ptr()),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let one = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                &name,
                w!("SYNTHETIC lease one"),
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
            let two = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                &name,
                w!("SYNTHETIC lease two"),
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
            let a = window::Id::unique();
            let b = window::Id::unique();
            assert!(register(a, one.0 as isize));
            assert!(register(b, two.0 as isize));
            assert!(!register(window::Id::unique(), two.0 as isize));
            unregister(a);
            assert!(!live(a));
            assert!(live(b));
            assert!(IsWindow(Some(two)).as_bool());
            DestroyWindow(one).unwrap();
            DestroyWindow(two).unwrap();
            assert!(!live(b));
            unregister(b);
            UnregisterClassW(&name, Some(instance)).unwrap();
        }
    }
}
