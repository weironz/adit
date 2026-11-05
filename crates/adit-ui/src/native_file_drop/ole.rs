use super::*;
use windows::core::{implement, Ref, Result as WinResult};
use windows::Win32::{
    Foundation::{E_INVALIDARG, HWND, LPARAM, LRESULT, POINT, POINTL, RECT, S_OK, WPARAM},
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
        WindowsAndMessaging::{GetClientRect, WM_DPICHANGED, WM_MOVE, WM_NCDESTROY, WM_SIZE},
    },
};

const SUBCLASS: usize = 0x4144_4452;
const MAX_PATH_UNITS: usize = 32767;

pub(super) fn dimensions(raw: isize) -> Option<Metrics> {
    let hwnd = HWND(raw as *mut _);
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let mut rect = RECT::default();
    unsafe {
        GetClientRect(hwnd, &mut rect).ok()?;
    }
    (dpi > 0 && rect.right > rect.left && rect.bottom > rect.top).then_some(Metrics {
        dpi,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    })
}

fn sample(raw: isize, screen: &POINTL) -> Option<(Metrics, i32, i32)> {
    let metrics = dimensions(raw)?;
    let mut client = POINT {
        x: screen.x,
        y: screen.y,
    };
    // OLE's POINTL is in physical screen pixels. iced's published regions are
    // logical client units; subtract the native client origin before scaling.
    if !unsafe { ScreenToClient(HWND(raw as *mut _), &mut client) }.as_bool() {
        return None;
    }
    Some((metrics, client.x, client.y))
}

fn hit(window: window::Id, raw: isize, point: &POINTL) -> Option<Lease> {
    let sample = sample(raw, point);
    shared().lock().ok()?.hover(window, raw, sample)
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
    let mut units = 0usize;
    for index in 0..count {
        let len = unsafe { DragQueryFileW(handle, index as u32, None) } as usize;
        if len == 0 || len > MAX_PATH_UNITS {
            return None;
        }
        units = units.checked_add(len + 1)?;
        if units > MAX_BATCH_UNITS {
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
    serial: std::cell::Cell<Option<u64>>,
}

impl OleTarget_Impl {
    fn effect(&self, point: &POINTL, effect: *mut DROPEFFECT) -> WinResult<()> {
        let effect = unsafe { effect.as_mut() }.ok_or(E_INVALIDARG)?;
        let copy_allowed = effect.0 & DROPEFFECT_COPY.0 != 0;
        *effect = DROPEFFECT_NONE;
        if !copy_allowed {
            return Ok(());
        }
        // Do not call into an incoming object at all over an RDP surface,
        // blocked modal, or another window's target.
        let Some(lease) = hit(self.window, self.hwnd, point) else {
            return Ok(());
        };
        let object = self.object.borrow().clone();
        if object.is_some_and(|object| unsafe { object.QueryGetData(&format()) == S_OK })
            && hit(self.window, self.hwnd, point) == Some(lease)
        {
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
        self.serial.set(
            shared()
                .lock()
                .ok()
                .and_then(|mut state| state.enter(self.window)),
        );
        let previous = self.object.replace(object.as_ref().cloned());
        drop(previous);
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
        let previous = self.object.replace(None);
        if let Some(serial) = self.serial.take() {
            if let Ok(mut state) = shared().lock() {
                state.leave(self.window, serial);
            }
        }
        drop(previous);
        Ok(())
    }
    fn Drop(
        &self,
        object: Ref<'_, IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        let serial = self.serial.take();
        let previous = self.object.replace(None);
        let result = self.drop_batch(object, point, effect, serial);
        if let Some(serial) = serial {
            if let Ok(mut state) = shared().lock() {
                state.leave(self.window, serial);
            }
        }
        drop(previous);
        result
    }
}

impl OleTarget_Impl {
    fn drop_batch(
        &self,
        object: Ref<'_, IDataObject>,
        point: &POINTL,
        effect: *mut DROPEFFECT,
        serial: Option<u64>,
    ) -> WinResult<()> {
        let effect = unsafe { effect.as_mut() }.ok_or(E_INVALIDARG)?;
        let copy_allowed = effect.0 & DROPEFFECT_COPY.0 != 0;
        *effect = DROPEFFECT_NONE;
        if !copy_allowed {
            return Ok(());
        }
        let Some(lease) =
            hit(self.window, self.hwnd, point).filter(|lease| Some(lease.serial) == serial)
        else {
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
        if hit(self.window, self.hwnd, point) == Some(lease.clone())
            && shared()
                .lock()
                .is_ok_and(|mut state| state.enqueue(self.window, lease, paths))
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
    if matches!(msg, WM_SIZE | WM_DPICHANGED | WM_MOVE) {
        let id = unsafe { (*(data as *const Registration)).window };
        invalidate(id);
    }
    if msg != WM_NCDESTROY {
        return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
    }
    let id = unsafe { (*(data as *const Registration)).window };
    let _ = unsafe { RemoveWindowSubclass(hwnd, Some(subclass), SUBCLASS) };
    let registration = WINDOWS.with(|windows| windows.borrow_mut().remove(&id));
    // Only our successful registration reaches this subclass. Revoke while the
    // HWND and its STA are still alive; never revoke another owner's target.
    let _ = unsafe { RevokeDragDrop(hwnd) };
    forget(id);
    let result = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
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
        serial: std::cell::Cell::new(None),
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
        state.install(window, raw);
    } else {
        let _ = unsafe { RemoveWindowSubclass(hwnd, Some(subclass), SUBCLASS) };
        let _ = unsafe { RevokeDragDrop(hwnd) };
        unsafe {
            OleUninitialize();
        }
        return false;
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

#[cfg(test)]
mod tests;
