//! A dedicated STA owns a small transfer window, its OLE registration, and the
//! modal DoDragDrop loop. iced keeps pumping tagged RDP chunk replies throughout.
//! Never register over winit's IDropTarget or pass COM pointers across threads.
use super::{Lease, Request};
use crate::clipboard_files::data_object::FileDataObject;
use crate::i18n::tf;
use adit_session::ClipFile;
use std::{
    cell::{Cell, RefCell},
    sync::{mpsc::Sender, OnceLock},
};
use windows::core::{implement, w, Ref, Result as WinResult, BOOL, HRESULT};
use windows::Win32::{
    Foundation::{
        DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, E_INVALIDARG, HANDLE,
        HWND, LPARAM, LRESULT, POINTL, RECT, S_OK, WPARAM,
    },
    Graphics::Gdi::{
        BeginPaint, DrawTextW, EndPaint, FillRect, GetStockObject, DT_CENTER, DT_WORDBREAK, HBRUSH,
        PAINTSTRUCT, WHITE_BRUSH,
    },
    System::{
        Com::IDataObject,
        LibraryLoader::GetModuleHandleW,
        Ole::{
            DoDragDrop, IDropSource, IDropSource_Impl, IDropTarget, IDropTarget_Impl,
            OleInitialize, OleUninitialize, RegisterDragDrop, RevokeDragDrop, DROPEFFECT,
            DROPEFFECT_COPY, DROPEFFECT_NONE, DROPEFFECT_SCROLL,
        },
        SystemServices::{MK_LBUTTON, MK_SHIFT, MODIFIERKEYS_FLAGS},
        Threading::{GetCurrentProcessId, GetCurrentThreadId},
    },
    UI::{
        Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture},
        WindowsAndMessaging::*,
    },
};

type Notices = Sender<(adit_domain::SessionId, String)>;

pub(super) fn open(
    owner: isize,
    title: String,
    files: Vec<ClipFile>,
    request: Request,
    lease: Lease,
    notices: Notices,
) -> Result<(), &'static str> {
    std::thread::Builder::new()
        .name("adit-ole-file-drag".into())
        .spawn(move || {
            let result = run(
                owner,
                title,
                files,
                request,
                lease.clone(),
                notices.clone(),
                true,
            );
            if result.is_err() {
                lease.cancel();
                let _ =
                    notices.send((lease.session, "无法打开原生文件拖放窗口；未传输文件".into()));
            }
        })
        .map(|_| ())
        .map_err(|_| "无法启动 OLE 文件拖放线程")
}

struct Sta;
impl Sta {
    fn new() -> WinResult<Self> {
        unsafe {
            OleInitialize(None)?;
        }
        Ok(Self)
    }
}
impl Drop for Sta {
    fn drop(&mut self) {
        unsafe {
            OleUninitialize();
        }
    }
}

/// This lease is deliberately !Send (IDropTarget is apartment bound). Only the
/// transfer window's STA creates and drops it, before destroying its HWND.
struct Registration {
    hwnd: HWND,
    thread: u32,
    tag: usize,
    _target: IDropTarget,
    _sta_only: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl Registration {
    fn new(hwnd: HWND, target: IDropTarget) -> WinResult<Self> {
        if unsafe { GetWindowThreadProcessId(hwnd, None) } != unsafe { GetCurrentThreadId() } {
            return Err(E_INVALIDARG.into());
        }
        unsafe {
            RegisterDragDrop(hwnd, &target)?;
        }
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
        let tag = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if unsafe { SetPropW(hwnd, w!("AditRdpDropLease"), Some(HANDLE(tag as *mut _))) }.is_err() {
            unsafe {
                let _ = RevokeDragDrop(hwnd);
            }
            return Err(E_INVALIDARG.into());
        }
        Ok(Self {
            hwnd,
            thread: unsafe { GetCurrentThreadId() },
            tag,
            _target: target,
            _sta_only: std::marker::PhantomData,
        })
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        debug_assert_eq!(self.thread, unsafe { GetCurrentThreadId() });
        unsafe {
            if GetWindowThreadProcessId(self.hwnd, None) == self.thread
                && GetPropW(self.hwnd, w!("AditRdpDropLease")) == HANDLE(self.tag as *mut _)
            {
                let _ = RevokeDragDrop(self.hwnd);
                let _ = RemovePropW(self.hwnd, w!("AditRdpDropLease"));
            }
        }
    }
}

#[implement(IDropSource)]
struct CopySource {
    lease: Lease,
}
impl IDropSource_Impl for CopySource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if !(self.lease.current)()
            || escape.as_bool()
            || keys.0 & MK_SHIFT.0 != 0
            || self
                .lease
                .cancelled
                .load(std::sync::atomic::Ordering::Acquire)
        {
            DRAGDROP_S_CANCEL
        } else if keys.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }
    fn GiveFeedback(&self, effect: DROPEFFECT) -> HRESULT {
        if effect.0 & !(DROPEFFECT_COPY.0 | DROPEFFECT_SCROLL.0) != 0 {
            E_INVALIDARG
        } else {
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }
}

/// CLIPRDR has no remote shell drop target. Refuse inbound drops explicitly:
/// accepting an offer and then synthesizing Ctrl+V would falsely report a drop.
#[implement(IDropTarget)]
struct InboundBoundary {
    lease: Lease,
    notices: Notices,
}
impl InboundBoundary {
    fn refuse(&self, effect: *mut DROPEFFECT) -> WinResult<()> {
        let effect =
            unsafe { effect.as_mut() }.ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
        *effect = DROPEFFECT_NONE;
        Ok(())
    }
}
impl IDropTarget_Impl for InboundBoundary_Impl {
    fn DragEnter(
        &self,
        _object: Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        let _ = self.notices.send((
            self.lease.session,
            "当前仅支持远端文件拖出；拖入远端尚不支持，未读取或传输文件".into(),
        ));
        self.refuse(effect)
    }
    fn DragOver(
        &self,
        _keys: MODIFIERKEYS_FLAGS,
        _point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        self.refuse(effect)
    }
    fn DragLeave(&self) -> WinResult<()> {
        Ok(())
    }
    fn Drop(
        &self,
        _object: Ref<'_, IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> WinResult<()> {
        self.refuse(effect)
    }
}

struct Context {
    owner: HWND,
    title: String,
    files: Vec<ClipFile>,
    request: Request,
    lease: Lease,
    notices: Notices,
    registration: RefCell<Option<Registration>>,
    origin: Cell<Option<(i32, i32)>>,
    dragging: Cell<bool>,
}
impl Context {
    fn announce(&self, text: &str) {
        let _ = self.notices.send((self.lease.session, text.into()));
    }
}

fn window_class() -> WinResult<()> {
    static CLASS: OnceLock<bool> = OnceLock::new();
    if *CLASS.get_or_init(|| unsafe {
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: GetModuleHandleW(None).map(|m| m.into()).unwrap_or_default(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: w!("AditExplicitRdpFileDrag"),
            ..Default::default()
        }) != 0
    }) {
        Ok(())
    } else {
        Err(E_INVALIDARG.into())
    }
}

fn owner_valid(owner: HWND) -> bool {
    let mut pid = 0;
    unsafe {
        IsWindow(Some(owner)).as_bool()
            && GetWindowThreadProcessId(owner, Some(&mut pid)) != 0
            && pid == GetCurrentProcessId()
    }
}

fn run(
    owner: isize,
    title: String,
    files: Vec<ClipFile>,
    request: Request,
    lease: Lease,
    notices: Notices,
    visible: bool,
) -> WinResult<()> {
    let _sta = Sta::new()?;
    let owner = HWND(owner as *mut _);
    if !owner_valid(owner) {
        return Err(E_INVALIDARG.into());
    }
    window_class()?;
    let context = Box::new(Context {
        owner,
        title: title.chars().filter(|c| !c.is_control()).take(80).collect(),
        files,
        request,
        lease,
        notices,
        registration: RefCell::new(None),
        origin: Cell::new(None),
        dragging: Cell::new(false),
    });
    unsafe {
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("AditExplicitRdpFileDrag"),
            w!("远端普通文件拖出 · 只复制"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            440,
            210,
            Some(owner),
            None,
            Some(GetModuleHandleW(None)?.into()),
            Some(&*context as *const Context as *const _),
        )?;
        let target: IDropTarget = InboundBoundary {
            lease: context.lease.clone(),
            notices: context.notices.clone(),
        }
        .into();
        match Registration::new(hwnd, target) {
            Ok(registration) => {
                *context.registration.borrow_mut() = Some(registration);
            }
            Err(error) => {
                let _ = DestroyWindow(hwnd);
                return Err(error);
            }
        }
        if SetTimer(Some(hwnd), 1, 100, None) == 0 {
            close(hwnd, &context);
            return Err(E_INVALIDARG.into());
        }
        if visible {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        context.announce("已打开文件拖出窗口；尚未读取文件内容");
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        if IsWindow(Some(hwnd)).as_bool() {
            close(hwnd, &context);
        }
    }
    Ok(())
}

unsafe fn close(hwnd: HWND, context: &Context) {
    context.lease.cancel();
    if !context.dragging.get()
        && unsafe { GetWindowThreadProcessId(hwnd, None) } == unsafe { GetCurrentThreadId() }
        && unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } == context as *const Context as isize
    {
        context.registration.borrow_mut().take();
        let _ = unsafe { DestroyWindow(hwnd) };
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Context;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        let context = &*ptr;
        match message {
            WM_CLOSE => {
                close(hwnd, context);
                LRESULT(0)
            }
            WM_TIMER => {
                if context
                    .lease
                    .cancelled
                    .load(std::sync::atomic::Ordering::Acquire)
                    || !owner_valid(context.owner)
                {
                    close(hwnd, context);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                context.lease.cancel();
                context.registration.borrow_mut().take();
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                context.origin.set(Some((
                    (lparam.0 as u16 as i16) as i32,
                    ((lparam.0 >> 16) as u16 as i16) as i32,
                )));
                SetCapture(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                context.origin.set(None);
                if GetCapture() == hwnd {
                    let _ = ReleaseCapture();
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED | WM_CANCELMODE | WM_KILLFOCUS => {
                context.origin.set(None);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                if let Some((x, y)) = context.origin.get() {
                    let nx = (lparam.0 as u16 as i16) as i32;
                    let ny = ((lparam.0 >> 16) as u16 as i16) as i32;
                    let held = wparam.0 & MK_LBUTTON.0 as usize != 0;
                    if drag_intent(
                        (x, y),
                        (nx, ny),
                        held,
                        GetCapture() == hwnd,
                        (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)),
                    ) {
                        context.origin.set(None);
                        let _ = ReleaseCapture();
                        start_drag(context);
                        close(hwnd, context);
                    } else if !held || GetCapture() != hwnd {
                        context.origin.set(None);
                    }
                }
                LRESULT(0)
            }
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut paint);
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                FillRect(dc, &rect, HBRUSH(GetStockObject(WHITE_BRUSH).0));
                rect.left += 16;
                rect.right -= 16;
                rect.top += 20;
                let label = tf(
                    "{}\n{} 个远端非空普通文件\n\n按住本窗口内容区，拖到资源管理器目标文件夹。\n仅复制；按 Esc 取消。\n拖入远端、目录和虚拟路径暂不支持。",
                    &[&context.title, &context.files.len()],
                );
                let mut text: Vec<u16> = label.encode_utf16().collect();
                DrawTextW(dc, &mut text, &mut rect, DT_CENTER | DT_WORDBREAK);
                let _ = EndPaint(hwnd, &paint);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }
}

fn drag_intent(
    origin: (i32, i32),
    current: (i32, i32),
    left_held: bool,
    owns_capture: bool,
    threshold: (i32, i32),
) -> bool {
    left_held
        && owns_capture
        && ((current.0 - origin.0).abs() >= threshold.0
            || (current.1 - origin.1).abs() >= threshold.1)
}

fn start_drag(context: &Context) {
    if context
        .lease
        .cancelled
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return;
    }
    let Ok(object) = FileDataObject::for_drag(
        context.files.clone(),
        context.lease.bridge.clone(),
        context.request.clone(),
        64 * 1024,
        context.lease.cancelled.clone(),
    ) else {
        context.announce("文件描述无效，已拒绝拖放");
        return;
    };
    let object: IDataObject = object.with_liveness(context.lease.current.clone()).into();
    let source: IDropSource = CopySource {
        lease: context.lease.clone(),
    }
    .into();
    let mut effect = DROPEFFECT_NONE;
    context.dragging.set(true);
    let result = unsafe { DoDragDrop(&object, &source, DROPEFFECT_COPY, &mut effect) };
    context.dragging.set(false);
    if result == DRAGDROP_S_DROP && effect == DROPEFFECT_COPY {
        context.announce("OLE 目标已接受文件复制；目标文件由资源管理器处理");
    } else if result == DRAGDROP_S_CANCEL {
        context.announce("文件拖放已取消");
    } else {
        context.announce("文件拖放未完成；未请求移动或删除远端文件");
    }
}

#[cfg(test)]
mod tests;
