//! Windows OLE file drops. The window is created with winit's target disabled;
//! this module never revokes a target it did not successfully register.
use super::*;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Mutex, OnceLock},
};
use windows::core::{implement, Ref, Result as WinResult};
use windows::Win32::{
    Foundation::{E_INVALIDARG, HWND, LPARAM, LRESULT, POINT, POINTL, RECT, WPARAM},
    Graphics::Gdi::ScreenToClient,
    System::{
        Com::{IDataObject, FORMATETC, STGMEDIUM},
        Ole::{
            IDropTarget, IDropTarget_Impl, OleInitialize, OleUninitialize, RegisterDragDrop,
            ReleaseStgMedium, RevokeDragDrop, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY,
            DROPEFFECT_NONE,
        },
        SystemServices::MODIFIERKEYS_FLAGS,
    },
    UI::{
        HiDpi::GetDpiForWindow,
        Shell::{DefSubclassProc, DragQueryFileW, RemoveWindowSubclass, SetWindowSubclass, HDROP},
        WindowsAndMessaging::{GetClientRect, WM_NCDESTROY},
    },
};

const SUBCLASS: usize = 0x4144_4452;
const MAX_FILES: usize = 4096;
const MAX_PATH_UNITS: usize = 32767;
const MAX_PENDING: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Destination {
    Shell(SessionId),
    Panel { instance: SessionId, cwd: String },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Region {
    pub destination: Destination,
    pub bounds: iced::Rectangle,
}

#[derive(Clone, Default)]
pub(crate) enum Policy {
    #[default]
    Disabled,
    Shells(HashSet<SessionId>),
    Panel {
        instance: SessionId,
        cwd: String,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum Mark {
    Shell(SessionId),
    Panel,
}

thread_local! {
    static FRAME: RefCell<Option<(Policy, Vec<Region>)>> = const { RefCell::new(None) };
}

pub(crate) fn begin_frame(policy: Policy) {
    FRAME.with(|frame| *frame.borrow_mut() = Some((policy, Vec::new())));
}

pub(crate) fn mark(mark: Mark, bounds: iced::Rectangle, viewport: iced::Rectangle) {
    let Some(bounds) = bounds.intersection(&viewport) else {
        return;
    };
    if bounds.width <= 0.0 || bounds.height <= 0.0 {
        return;
    }
    FRAME.with(|frame| {
        let mut frame = frame.borrow_mut();
        let Some((policy, regions)) = frame.as_mut() else {
            return;
        };
        let destination = match (policy, mark) {
            (Policy::Shells(sessions), Mark::Shell(session)) if sessions.contains(&session) => {
                Some(Destination::Shell(session))
            }
            (Policy::Panel { instance, cwd }, Mark::Panel) => Some(Destination::Panel {
                instance: *instance,
                cwd: cwd.clone(),
            }),
            _ => None,
        };
        if let Some(destination) = destination {
            regions.push(Region {
                destination,
                bounds,
            });
        }
    });
}

pub(crate) fn finish_frame(window: window::Id, bounds: iced::Rectangle) {
    let frame = FRAME.with(|frame| frame.borrow_mut().take());
    if let Some((_, regions)) = frame {
        publish(window, bounds, regions);
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Geometry {
    hwnd: isize,
    dpi: u32,
    width: i32,
    height: i32,
    regions: Vec<Region>,
}

#[derive(Default)]
struct Slot {
    hwnd: isize,
    generation: u64,
    geometry: Option<Geometry>,
    dragging: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct DropRequest {
    pub window: window::Id,
    pub destination: Destination,
    pub paths: Vec<PathBuf>,
    generation: u64,
}

#[derive(Default)]
struct Shared {
    slots: HashMap<window::Id, Slot>,
    pending: VecDeque<DropRequest>,
}

fn shared() -> &'static Mutex<Shared> {
    static STATE: OnceLock<Mutex<Shared>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(Shared::default()))
}

fn dimensions(hwnd: HWND) -> Option<(u32, i32, i32)> {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let mut rect = RECT::default();
    unsafe {
        GetClientRect(hwnd, &mut rect).ok()?;
    }
    (dpi > 0 && rect.right > rect.left && rect.bottom > rect.top).then_some((
        dpi,
        rect.right - rect.left,
        rect.bottom - rect.top,
    ))
}

pub(crate) fn publish(window: window::Id, bounds: iced::Rectangle, regions: Vec<Region>) {
    let Ok(mut state) = shared().lock() else {
        return;
    };
    let Some(slot) = state.slots.get_mut(&window) else {
        return;
    };
    let hwnd = HWND(slot.hwnd as *mut _);
    let Some((dpi, width, height)) = dimensions(hwnd) else {
        slot.geometry = None;
        return;
    };
    let scale = dpi as f32 / 96.0;
    // Reject a stale layout or a scale factor that iced and Win32 disagree on.
    if (bounds.width * scale - width as f32).abs() > 2.0
        || (bounds.height * scale - height as f32).abs() > 2.0
    {
        slot.geometry = None;
        slot.generation = slot.generation.wrapping_add(1);
        return;
    }
    let next = Geometry {
        hwnd: slot.hwnd,
        dpi,
        width,
        height,
        regions,
    };
    if slot.geometry.as_ref() != Some(&next) {
        slot.generation = slot.generation.wrapping_add(1);
        slot.geometry = Some(next);
    }
}

pub(crate) fn invalidate(window: window::Id) {
    if let Ok(mut state) = shared().lock() {
        if let Some(slot) = state.slots.get_mut(&window) {
            slot.generation = slot.generation.wrapping_add(1);
            slot.geometry = None;
        }
    }
}

pub(crate) fn invalidate_all() {
    if let Ok(mut state) = shared().lock() {
        for slot in state.slots.values_mut() {
            slot.generation = slot.generation.wrapping_add(1);
            slot.geometry = None;
        }
    }
}

pub(crate) fn has_interest() -> bool {
    shared()
        .lock()
        .map(|state| !state.pending.is_empty() || state.slots.values().any(|slot| slot.dragging))
        .unwrap_or(false)
}

fn set_dragging(window: window::Id, dragging: bool) {
    if let Ok(mut state) = shared().lock() {
        if let Some(slot) = state.slots.get_mut(&window) {
            slot.dragging = dragging;
        }
    }
}

fn hit(window: window::Id, raw: isize, point: &POINTL) -> Option<(u64, Destination)> {
    let state = shared().lock().ok()?;
    let slot = state.slots.get(&window)?;
    let geometry = slot.geometry.as_ref()?;
    if slot.hwnd != raw || geometry.hwnd != raw {
        return None;
    }
    let hwnd = HWND(raw as *mut _);
    if dimensions(hwnd)? != (geometry.dpi, geometry.width, geometry.height) {
        return None;
    }
    let mut client = POINT {
        x: point.x,
        y: point.y,
    };
    if !unsafe { ScreenToClient(hwnd, &mut client) }.as_bool() {
        return None;
    }
    let scale = geometry.dpi as f32 / 96.0;
    let point = Point::new(client.x as f32 / scale, client.y as f32 / scale);
    let mut matches = geometry
        .regions
        .iter()
        .filter(|region| region.bounds.contains(point));
    let target = matches.next()?.destination.clone();
    if matches.next().is_some() {
        return None;
    }
    Some((slot.generation, target))
}

fn enqueue(
    window: window::Id,
    generation: u64,
    destination: Destination,
    paths: Vec<PathBuf>,
) -> bool {
    let Ok(mut state) = shared().lock() else {
        return false;
    };
    if state.pending.len() >= MAX_PENDING
        || !state
            .slots
            .get(&window)
            .is_some_and(|slot| slot.generation == generation && slot.geometry.is_some())
    {
        return false;
    }
    state.pending.push_back(DropRequest {
        window,
        destination,
        paths,
        generation,
    });
    true
}

pub(crate) fn drain() -> Vec<DropRequest> {
    let Ok(mut state) = shared().lock() else {
        return Vec::new();
    };
    let pending = std::mem::take(&mut state.pending);
    pending
        .into_iter()
        .filter(|request| {
            state.slots.get(&request.window).is_some_and(|slot| {
                slot.generation == request.generation && slot.geometry.is_some()
            })
        })
        .collect()
}

fn format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: 1,
        lindex: -1,
        tymed: 1,
    }
}

struct Medium(STGMEDIUM);
impl Drop for Medium {
    fn drop(&mut self) {
        unsafe {
            ReleaseStgMedium(&mut self.0);
        }
    }
}

fn paths(object: &IDataObject) -> Option<Vec<PathBuf>> {
    use std::os::windows::ffi::OsStringExt;
    let medium = Medium(unsafe { object.GetData(&format()).ok()? });
    if medium.0.tymed != 1 {
        return None;
    }
    let handle = HDROP(unsafe { medium.0.u.hGlobal.0 });
    if handle.0.is_null() {
        return None;
    }
    let count = unsafe { DragQueryFileW(handle, u32::MAX, None) } as usize;
    if count == 0 || count > MAX_FILES {
        return None;
    }
    let mut result = Vec::with_capacity(count);
    for index in 0..count {
        let len = unsafe { DragQueryFileW(handle, index as u32, None) } as usize;
        if len == 0 || len > MAX_PATH_UNITS {
            return None;
        }
        let mut buffer = vec![0; len + 1];
        let written = unsafe { DragQueryFileW(handle, index as u32, Some(&mut buffer)) } as usize;
        if written != len {
            return None;
        }
        result.push(std::ffi::OsString::from_wide(&buffer[..len]).into());
    }
    Some(result)
}

#[implement(IDropTarget)]
struct OleTarget {
    window: window::Id,
    hwnd: isize,
    object: RefCell<Option<IDataObject>>,
}

impl OleTarget_Impl {
    fn effect(&self, point: &POINTL, effect: *mut DROPEFFECT) -> WinResult<()> {
        let effect = unsafe { effect.as_mut() }.ok_or(E_INVALIDARG)?;
        let copy_allowed = effect.0 & DROPEFFECT_COPY.0 != 0;
        *effect = DROPEFFECT_NONE;
        if !copy_allowed || hit(self.window, self.hwnd, point).is_none() {
            return Ok(());
        }
        let object = self.object.borrow().clone();
        if object.is_some_and(|object| unsafe { object.QueryGetData(&format()).is_ok() }) {
            *effect = DROPEFFECT_COPY;
        }
        Ok(())
    }
}

impl IDropTarget_Impl for OleTarget_Impl {
    fn DragEnter(
        &self,
        object: Ref<'_, IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        set_dragging(self.window, true);
        *self.object.borrow_mut() = object.as_ref().cloned();
        self.effect(point, effect)
    }
    fn DragOver(
        &self,
        _: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        self.effect(point, effect)
    }
    fn DragLeave(&self) -> WinResult<()> {
        self.object.borrow_mut().take();
        set_dragging(self.window, false);
        Ok(())
    }
    fn Drop(
        &self,
        object: Ref<'_, IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        self.object.borrow_mut().take();
        set_dragging(self.window, false);
        let effect = unsafe { effect.as_mut() }.ok_or(E_INVALIDARG)?;
        let copy_allowed = effect.0 & DROPEFFECT_COPY.0 != 0;
        *effect = DROPEFFECT_NONE;
        if !copy_allowed {
            return Ok(());
        }
        let Some((generation, destination)) = hit(self.window, self.hwnd, point) else {
            return Ok(());
        };
        let Some(object) = object.as_ref() else {
            return Ok(());
        };
        let Some(paths) = paths(object) else {
            return Ok(());
        };
        // IDataObject is foreign code and can re-enter the window; verify the
        // geometry lease again after it returns before accepting the batch.
        if hit(self.window, self.hwnd, point) == Some((generation, destination.clone()))
            && enqueue(self.window, generation, destination, paths)
        {
            *effect = DROPEFFECT_COPY;
        }
        Ok(())
    }
}

struct Registration {
    window: window::Id,
    _target: IDropTarget,
}
thread_local! { static WINDOWS: RefCell<HashMap<window::Id, Box<Registration>>> = RefCell::new(HashMap::new()); }

unsafe extern "system" fn subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    data: usize,
) -> LRESULT {
    if msg != WM_NCDESTROY {
        return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
    }
    let id = unsafe { (*(data as *const Registration)).window };
    let result = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
    let _ = unsafe { RemoveWindowSubclass(hwnd, Some(subclass), SUBCLASS) };
    let registration = WINDOWS.with(|windows| windows.borrow_mut().remove(&id));
    if let Ok(mut state) = shared().lock() {
        state.slots.remove(&id);
        state.pending.retain(|item| item.window != id);
    }
    drop(registration);
    unsafe {
        OleUninitialize();
    }
    result
}

fn register(window: window::Id, raw: isize) -> bool {
    if raw == 0 || WINDOWS.with(|windows| windows.borrow().contains_key(&window)) {
        return false;
    }
    if unsafe { OleInitialize(None) }.is_err() {
        return false;
    }
    let hwnd = HWND(raw as *mut _);
    let target: IDropTarget = OleTarget {
        window,
        hwnd: raw,
        object: RefCell::new(None),
    }
    .into();
    if unsafe { RegisterDragDrop(hwnd, &target) }.is_err() {
        // An existing target belongs to somebody else. Do not revoke it.
        unsafe {
            OleUninitialize();
        }
        return false;
    }
    let registration = Box::new(Registration {
        window,
        _target: target,
    });
    let ptr = (&*registration as *const Registration) as usize;
    if !unsafe { SetWindowSubclass(hwnd, Some(subclass), SUBCLASS, ptr) }.as_bool() {
        let _ = unsafe { RevokeDragDrop(hwnd) };
        unsafe {
            OleUninitialize();
        }
        return false;
    }
    if let Ok(mut state) = shared().lock() {
        state.slots.insert(
            window,
            Slot {
                hwnd: raw,
                ..Slot::default()
            },
        );
    }
    WINDOWS.with(|windows| {
        windows.borrow_mut().insert(window, registration);
    });
    true
}

pub(crate) fn register_task(window: window::Id) -> Task<bool> {
    window::run(window, move |native| {
        use iced::window::raw_window_handle::RawWindowHandle;
        match native.window_handle().map(|handle| handle.as_raw()) {
            Ok(RawWindowHandle::Win32(handle)) => register(window, handle.hwnd.get()),
            _ => false,
        }
    })
}
