//! Official AccessKit UIA adapter with a chain-safe, per-HWND subclass.
//! Installation runs through iced::window::run on the owning window thread.
use super::*;
use accesskit::{Action, ActionData, ActionRequest, Node, NodeId, Role, TreeId, TreeInfo, TreeUpdate};
use accesskit_windows::Adapter;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::{mpsc, Arc, Mutex},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::{
        HiDpi::GetDpiForWindow,
        Input::KeyboardAndMouse::GetFocus,
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            GetWindowThreadProcessId, IsWindow, WM_GETOBJECT, WM_KILLFOCUS,
            WM_NCDESTROY, WM_SETFOCUS,
        },
    },
};

const ROOT: NodeId = NodeId(1);
const SUBCLASS: usize = 0x4144_4954;

#[derive(Clone)]
struct Route {
    invoke: Option<Message>,
    set_value: Option<fn(String) -> Message>,
    focus: Option<iced::advanced::widget::Id>,
}

struct Shared {
    tree: Option<TreeUpdate>,
    routes: HashMap<NodeId, Route>,
    namespace: String,
    revoked: bool,
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
}

struct Actions {
    state: Arc<Mutex<Shared>>,
    tx: mpsc::SyncSender<Request>,
}

impl accesskit::ActionHandler for Actions {
    fn do_action(&mut self, request: ActionRequest) {
        if request.target_tree != TreeId::ROOT { return; }
        let Ok(state) = self.state.lock() else { return };
        if state.revoked { return; }
        let Some(route) = state.routes.get(&request.target_node) else { return };
        let mut out = Request {
            node: request.target_node, namespace: state.namespace.clone(),
            action: request.action, value: None,
        };
        match (request.action, request.data) {
            (Action::Focus, _) if route.focus.is_some() => {}
            (Action::Click, _) if route.invoke.is_some() => {}
            (Action::SetValue, Some(ActionData::Value(value))) if route.set_value.is_some() && value.len() <= 8192 => {
                out.value = Some(value.into());
            }
            _ => return,
        }
        let _ = self.tx.try_send(out);
    }
}

struct Window {
    hwnd: HWND,
    alive: Cell<bool>,
    adapter: RefCell<Adapter>,
    shared: Arc<Mutex<Shared>>,
    rx: mpsc::Receiver<Request>,
    ids: HashMap<String, NodeId>,
    next_id: u64,
    last: Option<TreeUpdate>,
}

thread_local! {
    static WINDOWS: RefCell<HashMap<window::Id, Box<Window>>> = RefCell::new(HashMap::new());
}

unsafe extern "system" fn subclass(
    hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM, _: usize, data: usize,
) -> LRESULT {
    // The Box remains pinned in WINDOWS until the subclass has been removed.
    let window = unsafe { &*(data as *const Window) };
    match message {
        WM_GETOBJECT => {
            let mut activate = Activate(Arc::clone(&window.shared));
            let result = window.adapter.borrow_mut().handle_wm_getobject(wparam, lparam, &mut activate);
            if let Some(result) = result { return result.into(); }
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            let events = window.adapter.borrow_mut().update_window_focus_state(message == WM_SETFOCUS);
            if let Some(events) = events { events.raise(); }
        }
        WM_NCDESTROY => {
            window.alive.set(false);
            if let Ok(mut state) = window.shared.lock() {
                state.revoked = true;
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
    if unsafe { GetWindowThreadProcessId(hwnd, None) != GetCurrentThreadId() } { return false; }
    WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        windows.retain(|_, window| window.alive.get());
        if let Some(existing) = windows.get(&id) { return existing.hwnd == hwnd; }
        if windows.values().any(|window| window.hwnd == hwnd) { return false; }
        let shared = Arc::new(Mutex::new(Shared {
            tree: None, routes: HashMap::new(), namespace: String::new(), revoked: false,
        }));
        let (tx, rx) = mpsc::sync_channel(64);
        let window = Box::new(Window {
            hwnd, alive: Cell::new(true),
            adapter: RefCell::new(Adapter::new(
                hwnd, unsafe { GetFocus() == hwnd },
                Actions { state: Arc::clone(&shared), tx },
            )),
            shared, rx, ids: HashMap::new(), next_id: 2, last: None,
        });
        let pointer = (&*window as *const Window) as usize;
        if !unsafe { SetWindowSubclass(hwnd, Some(subclass), SUBCLASS, pointer) }.as_bool() { return false; }
        windows.insert(id, window);
        true
    })
}

pub(super) fn unregister(id: window::Id) {
    let removed = WINDOWS.with(|windows| windows.borrow_mut().remove(&id));
    if let Some(window) = removed {
        if let Ok(mut state) = window.shared.lock() {
            state.revoked = true;
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
            nodes: vec![(ROOT, root)], tree: Some(TreeInfo::new(ROOT)),
            tree_id: TreeId::ROOT, focus: ROOT,
        });
        if let Some(events) = events { events.raise(); }
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
        let Some(window) = windows.get_mut(&window) else { return ROOT };
        *window.ids.entry(key.to_owned()).or_insert_with(|| {
            let id = NodeId(window.next_id);
            window.next_id += 1;
            id
        })
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
    id: window::Id, namespace: String, rendered: Vec<Rendered>, layer: usize, bounds: iced::Rectangle,
) {
    let events = WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        let window = windows.get_mut(&id)?;
        if !window.alive.get() { return None; }
        let scale = f64::from(unsafe { GetDpiForWindow(window.hwnd) }) / 96.0;
        let rect = |r: iced::Rectangle| accesskit::Rect::new(
            f64::from(r.x) * scale, f64::from(r.y) * scale,
            f64::from(r.x + r.width) * scale, f64::from(r.y + r.height) * scale,
        );
        let visible: Vec<_> = rendered.into_iter().filter(|r| r.layer == layer).collect();
        let valid: HashSet<_> = visible.iter().map(|r| r.id).collect();
        let mut nodes = HashMap::new();
        let mut children: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        let mut routes = HashMap::new();
        let mut focused = ROOT;
        for item in visible {
            let control = item.control;
            let role = match control.kind {
                Kind::Button => Role::Button, Kind::Item => Role::ListItem,
                Kind::Edit => Role::TextInput, Kind::Protected => Role::PasswordInput,
                Kind::Document => Role::Terminal, Kind::Range => Role::ScrollBar,
            };
            let mut node = Node::new(role);
            node.set_label(control.label);
            node.set_author_id(format!("{namespace}/{}", control.key));
            node.set_bounds(rect(item.bounds));
            if control.focus.is_some() { node.add_action(Action::Focus); }
            if item.focused { focused = item.id; }
            if matches!(control.kind, Kind::Item | Kind::Edit | Kind::Protected | Kind::Document) {
                node.set_value(if matches!(control.kind, Kind::Protected) {
                    if control.value.is_empty() { String::new() } else { "••••".into() }
                } else { control.value.clone() });
            }
            if matches!(control.kind, Kind::Item | Kind::Protected | Kind::Range) { node.set_read_only(); }
            if matches!(control.kind, Kind::Button) && control.invoke.is_none() { node.set_disabled(); }
            if control.invoke.is_some() { node.add_action(Action::Click); }
            if control.set_value.is_some() { node.add_action(Action::SetValue); }
            if let Some((_, value, max)) = control.range {
                node.set_numeric_value(value as f64);
                node.set_min_numeric_value(0.0);
                node.set_max_numeric_value(max as f64);
                node.set_numeric_value_step(1.0);
            }
            if matches!(control.kind, Kind::Document) {
                let key = format!("{namespace}/{}/text", control.key);
                let text_id = *window.ids.entry(key).or_insert_with(|| {
                    let id = NodeId(window.next_id); window.next_id += 1; id
                });
                let mut run = Node::new(Role::TextRun);
                run.set_bounds(rect(item.bounds));
                run.set_character_lengths(control.value.chars().map(|c| c.len_utf8() as u8).collect::<Vec<_>>());
                run.set_value(control.value);
                node.set_read_only();
                children.entry(item.id).or_default().push(text_id);
                nodes.insert(text_id, run);
            }
            routes.insert(item.id, Route { invoke: control.invoke, set_value: control.set_value, focus: control.focus });
            let parent = if valid.contains(&item.parent) { item.parent } else { ROOT };
            children.entry(parent).or_default().push(item.id);
            nodes.insert(item.id, node);
        }
        let mut root = Node::new(Role::Window);
        root.set_label("Adit");
        root.set_bounds(rect(bounds));
        nodes.insert(ROOT, root);
        for (parent, children) in children {
            if let Some(node) = nodes.get_mut(&parent) { node.set_children(children); }
        }
        let mut nodes: Vec<_> = nodes.into_iter().collect();
        nodes.sort_by_key(|(id, _)| id.0);
        let tree = TreeUpdate { nodes, tree: Some(TreeInfo::new(ROOT)), tree_id: TreeId::ROOT, focus: focused };
        if let Ok(mut state) = window.shared.lock() {
            state.namespace = namespace;
            state.routes = routes;
            state.tree = Some(tree.clone());
        }
        if window.last.as_ref() == Some(&tree) { return None; }
        window.last = Some(tree.clone());
        let events = window.adapter.borrow_mut().update_if_active(|| tree);
        events
    });
    // UIA event raising may re-enter the HWND. No registry/adapter borrow is
    // held here, so WM_GETOBJECT can synchronously obtain the current tree.
    if let Some(events) = events { events.raise(); }
}

pub(super) fn live(id: window::Id) -> bool {
    WINDOWS.with(|windows| windows.borrow().get(&id).is_some_and(|window| window.alive.get()))
}

fn current(state: &Shared, request: &Request, namespace: &str) -> bool {
    !state.revoked && request.namespace == namespace && state.namespace == namespace
}

pub(super) fn drain(registered: &HashSet<window::Id>, namespace: &str) -> Vec<(window::Id, String, Message)> {
    WINDOWS.with(|windows| {
        let windows = windows.borrow();
        let mut messages = Vec::new();
        for (id, window) in windows.iter().filter(|(id, _)| registered.contains(id)) {
            for _ in 0..32 {
                let Ok(request) = window.rx.try_recv() else { break };
                let Ok(state) = window.shared.lock() else { continue };
                if !current(&state, &request, namespace) { continue; }
                let Some(route) = state.routes.get(&request.node) else { continue };
                let message = match request.action {
                    Action::Focus => route.focus.clone().map(|focus| Message::AccessibilityFocus { window: *id, id: focus }),
                    Action::Click => route.invoke.clone(),
                    Action::SetValue if route.set_value.is_some() =>
                        request.value.map(|value| (route.set_value.unwrap())(value)),
                    _ => None,
                };
                if let Some(message) = message { messages.push((*id, request.namespace, message)); }
            }
        }
        messages
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler(route: Route) -> (Actions, mpsc::Receiver<Request>) {
        let (tx, rx) = mpsc::sync_channel(64);
        let shared = Shared {
            tree: None, routes: HashMap::from([(NodeId(2), route)]),
            namespace: "editor:fixture".into(), revoked: false,
        };
        (Actions { state: Arc::new(Mutex::new(shared)), tx }, rx)
    }

    fn request(action: Action, data: Option<ActionData>) -> ActionRequest {
        ActionRequest { action, target_tree: TreeId::ROOT, target_node: NodeId(2), data }
    }

    #[test]
    fn protected_actions_accept_only_explicit_focus_and_never_queue_values() {
        use accesskit::ActionHandler;
        let (mut handler, rx) = handler(Route {
            invoke: None, set_value: None,
            focus: Some(iced::advanced::widget::Id::new("fixture-password")),
        });
        handler.do_action(request(Action::SetValue, Some(ActionData::Value("SYNTHETIC-SECRET".into()))));
        assert!(rx.try_recv().is_err());
        handler.do_action(request(Action::Focus, None));
        let focus = rx.try_recv().unwrap();
        assert_eq!(focus.action, Action::Focus);
        assert!(focus.value.is_none());
    }

    #[test]
    fn queues_are_bounded_and_old_modal_or_revoked_actions_are_rejected() {
        use accesskit::ActionHandler;
        let (mut handler, rx) = handler(Route {
            invoke: Some(Message::CloseProfileEditor), set_value: None, focus: None,
        });
        for _ in 0..100 { handler.do_action(request(Action::Click, None)); }
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
                Foundation::HINSTANCE, System::LibraryLoader::GetModuleHandleW,
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
            let class = WNDCLASSW { hInstance: instance, lpfnWndProc: Some(proc),
                lpszClassName: windows::core::PCWSTR(name.as_ptr()), ..Default::default() };
            assert_ne!(RegisterClassW(&class), 0);
            let one = CreateWindowExW(WINDOW_EX_STYLE::default(), &name, w!("SYNTHETIC lease one"),
                WS_OVERLAPPEDWINDOW, 0, 0, 200, 100, None, None, Some(instance), None).unwrap();
            let two = CreateWindowExW(WINDOW_EX_STYLE::default(), &name, w!("SYNTHETIC lease two"),
                WS_OVERLAPPEDWINDOW, 0, 0, 200, 100, None, None, Some(instance), None).unwrap();
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
