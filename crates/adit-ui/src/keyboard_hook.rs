//! Passing global hotkeys through to a fullscreen remote desktop.
//!
//! ## Why a hook is the only way
//!
//! A hotkey another application registered with `RegisterHotKey` — a screenshot
//! tool on Ctrl+Shift+X, say — is dispatched by Windows *before* any window sees
//! the keystroke. Adit never receives the event, so there is nothing to forward:
//! the remote desktop simply never learns the key was pressed. Nor is this
//! special to Adit; no RDP client can forward what it is not given.
//!
//! What mstsc does, and what this does, is install a **low-level keyboard hook**
//! (`WH_KEYBOARD_LL`). That runs ahead of hotkey dispatch, so the key can be
//! swallowed and re-sent as a scancode to the remote instead.
//!
//! ## Why it is kept on a very short leash
//!
//! A low-level hook is global: its callback runs for every keystroke on the
//! machine, on the system input thread, and swallowing the wrong thing makes the
//! whole desktop feel broken rather than just this window. Three rules follow,
//! and all three are load-bearing:
//!
//! * **Armed only while fullscreen.** That is the mode where the desktop owns
//!   the screen and the user plainly means keys for the remote. Leaving it
//!   disarms, so ordinary local work is never affected.
//! * **The callback does the minimum.** It reads an atomic and pushes onto a
//!   channel. No allocation, no locking, no blocking — Windows gives the
//!   callback a deadline (`LowLevelHooksTimeout`) and silently removes hooks
//!   that miss it, so a slow callback does not fail loudly, it just stops
//!   working.
//! * **It never swallows the way out.** Ctrl+Alt+Enter leaves fullscreen and is
//!   passed through deliberately; without that, a fullscreen session whose
//!   toolbar the user has not found could only be escaped by killing Adit.

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Mutex, OnceLock};

    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT,
        WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    /// Whether the hook should currently swallow and forward. Read on the system
    /// input thread for every keystroke, so it is an atomic and nothing more.
    static ARMED: AtomicBool = AtomicBool::new(false);

    /// Keystrokes the callback swallowed, waiting for the UI thread to forward
    /// them. A channel rather than a lock the callback might contend on: the
    /// send is wait-free in the uncontended case, and the callback must never
    /// block the system input thread.
    static QUEUE: OnceLock<(Sender<Stroke>, Mutex<Receiver<Stroke>>)> = OnceLock::new();

    static HOOK: Mutex<Option<isize>> = Mutex::new(None);

    /// One swallowed key, already in the form the RDP wire wants.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct Stroke {
        pub(crate) scancode: u8,
        pub(crate) extended: bool,
        pub(crate) pressed: bool,
    }

    fn queue() -> &'static (Sender<Stroke>, Mutex<Receiver<Stroke>>) {
        QUEUE.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            (tx, Mutex::new(rx))
        })
    }

    /// Whether Ctrl and Alt are both physically down, asked of the OS rather
    /// than tracked here — the hook misses releases while disarmed, and a
    /// modifier believed stuck would swallow the escape hatch forever.
    ///
    /// # Safety
    /// `GetAsyncKeyState` takes a virtual-key code and is safe to call at any
    /// time; the `unsafe` is only the FFI boundary.
    unsafe fn ctrl_and_alt_down() -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_MENU};
        let down = |vk: u16| (GetAsyncKeyState(i32::from(vk)) as u16 & 0x8000) != 0;
        down(VK_CONTROL.0) && down(VK_MENU.0)
    }

    /// The hook itself. Runs on the system input thread for **every** keystroke
    /// on the machine, so it does as little as is possible.
    ///
    /// # Safety
    /// Called by Windows with `lparam` pointing at a `KBDLLHOOKSTRUCT` whenever
    /// `code == HC_ACTION` (0).
    unsafe extern "system" fn callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // Anything but HC_ACTION must be passed on untouched, per the API
        // contract — and the struct is only valid then.
        if code != 0 || !ARMED.load(Ordering::Relaxed) {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);

        // Injected keys are left alone: `SendInput` from another tool would
        // otherwise loop back through here.
        const LLKHF_INJECTED: u32 = 0x10;
        if info.flags.0 & LLKHF_INJECTED != 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        let pressed = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let released = matches!(wparam.0 as u32, WM_KEYUP | WM_SYSKEYUP);
        if !pressed && !released {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // The way out of fullscreen has to keep working locally.
        const VK_RETURN: u32 = 0x0D;
        if info.vkCode == VK_RETURN && ctrl_and_alt_down() {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        const LLKHF_EXTENDED: u32 = 0x01;
        let stroke = Stroke {
            scancode: (info.scanCode & 0xFF) as u8,
            extended: info.flags.0 & LLKHF_EXTENDED != 0,
            pressed,
        };
        // A failed send means the receiver is gone, i.e. we are shutting down;
        // passing the key on is the right thing then.
        if queue().0.send(stroke).is_err() {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        // Swallowed: a non-zero return stops the key reaching hotkey dispatch
        // and every window below.
        LRESULT(1)
    }

    /// Install the hook if it is not already installed.
    pub(crate) fn arm() {
        let mut held = HOOK.lock().unwrap_or_else(|e| e.into_inner());
        if held.is_some() {
            ARMED.store(true, Ordering::Relaxed);
            return;
        }
        // SAFETY: a `WH_KEYBOARD_LL` hook takes no module handle and no thread
        // id, and the callback has the required signature. Removed in `disarm`.
        match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(callback), None, 0) } {
            Ok(hook) => {
                *held = Some(hook.0 as isize);
                ARMED.store(true, Ordering::Relaxed);
            }
            // No hook is a working app with local hotkeys, not a broken one, so
            // a failure simply leaves it unarmed.
            Err(_) => ARMED.store(false, Ordering::Relaxed),
        }
    }

    /// Remove the hook. Safe to call when nothing is installed.
    pub(crate) fn disarm() {
        ARMED.store(false, Ordering::Relaxed);
        let mut held = HOOK.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(raw) = held.take() {
            // SAFETY: the handle came from `SetWindowsHookExW` above and is
            // removed exactly once.
            unsafe {
                let _ = UnhookWindowsHookEx(HHOOK(raw as *mut core::ffi::c_void));
            }
        }
        // Anything swallowed but not yet forwarded is dropped: it was meant for
        // a session no longer in the mode that captured it, and replaying it
        // afterwards would fire keys the user cannot see land.
        if let Ok(rx) = queue().1.lock() {
            while rx.try_recv().is_ok() {}
        }
    }

    /// Take everything swallowed since the last call.
    pub(crate) fn drain() -> Vec<Stroke> {
        let Ok(rx) = queue().1.lock() else {
            return Vec::new();
        };
        rx.try_iter().collect()
    }
}

#[cfg(not(windows))]
mod imp {
    /// The hook is a Win32 mechanism and RDP ships on Windows only, so the other
    /// platforms get a shape that compiles and does nothing.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct Stroke {
        pub(crate) scancode: u8,
        pub(crate) extended: bool,
        pub(crate) pressed: bool,
    }

    pub(crate) fn arm() {}
    pub(crate) fn disarm() {}
    pub(crate) fn drain() -> Vec<Stroke> {
        Vec::new()
    }
}

// `Stroke` stays inside the module: callers read its fields off whatever
// `drain` hands back and never name the type.
pub(crate) use imp::{arm, disarm, drain};
