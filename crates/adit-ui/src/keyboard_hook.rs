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
//! machine, on the installing thread, and swallowing the wrong thing makes the
//! whole desktop feel broken rather than just this window. Three rules follow,
//! and all three are load-bearing:
//!
//! * **Armed only for the focused remote.** RDP uses fullscreen; VNC has an
//!   explicit capture switch. Focus loss disarms, and the callback also checks
//!   its target HWND so a stalled UI cannot steal another application's keys.
//! * **The callback does the minimum.** It reads an atomic and pushes onto a
//!   channel. It never waits for UI or network work — Windows gives the
//!   callback a deadline (`LowLevelHooksTimeout`) and silently removes hooks
//!   that miss it, so a slow callback does not fail loudly, it just stops
//!   working.
//! * **It never swallows the way out.** Ctrl+Alt+Enter leaves fullscreen and is
//!   passed through deliberately; without that, a fullscreen session whose
//!   toolbar the user has not found could only be escaped by killing Adit.

#[cfg(windows)]
#[path = "keyboard_raw_input.rs"]
mod raw_input;

// Standalone native probes include this shipping source without a VNC router;
// non-Windows builds retain only its value types for the shared input boundary.
#[cfg_attr(any(not(windows), feature = "native-vnc-acceptance"), allow(dead_code))]
#[path = "keyboard_vnc_chord.rs"]
pub(crate) mod vnc_chord;

#[cfg_attr(any(not(windows), feature = "native-vnc-acceptance"), allow(dead_code))]
#[path = "keyboard_vnc_order.rs"]
pub(crate) mod vnc_order;

#[cfg(windows)]
mod imp {
    use super::raw_input::KeyboardRegistrationLease;
    use super::vnc_order::{Entry, ModifierEdge, OrderedQueue};
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW,
        GetWindowThreadProcessId, KillTimer, PeekMessageW, PostThreadMessageW, SetTimer,
        SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG,
        PM_NOREMOVE, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_NULL, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    /// Whether the hook should currently swallow and forward. Read on the hook
    /// thread for every keystroke, so it is an atomic and nothing more.
    static ARMED: AtomicBool = AtomicBool::new(false);
    static VNC: AtomicBool = AtomicBool::new(false);
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    static TARGET_WINDOW: AtomicIsize = AtomicIsize::new(0);
    #[cfg(all(test, feature = "native-vnc-acceptance"))]
    pub(super) static FAIL_NEXT_INSTALL: AtomicBool = AtomicBool::new(false);
    #[cfg(feature = "native-vnc-acceptance")]
    static PUMP_TURNS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    #[cfg(feature = "native-vnc-acceptance")]
    static PUMP_EXIT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    thread_local! {
        // An asynchronously retiring hook must never capture for the next tab.
        static WORKER_GENERATION: Cell<u64> = const { Cell::new(0) };
        static CHORDS: RefCell<super::vnc_chord::Ownership> =
            RefCell::new(super::vnc_chord::Ownership::new([false; 512], None));
        static WINDOW_MODIFIERS: RefCell<[bool; 512]> = const { RefCell::new([false; 512]) };
    }

    // Synthetic native acceptance only: count delivery without logging keys or
    // changing injected-key filtering. An installed handle alone proves no input.
    #[cfg(feature = "native-vnc-acceptance")]
    static COUNTERS: [std::sync::atomic::AtomicU32; 5] =
        [const { std::sync::atomic::AtomicU32::new(0) }; 5];
    #[cfg(feature = "native-vnc-acceptance")]
    static PROBE_EVENTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    #[cfg(feature = "native-vnc-acceptance")]
    pub(crate) fn probe_input_tag() -> usize {
        0x4144_4954 ^ std::process::id() as usize
    }

    #[cfg(feature = "native-vnc-acceptance")]
    pub(crate) fn probe_events() -> u32 {
        PROBE_EVENTS.load(Ordering::Relaxed)
    }

    #[cfg(feature = "native-vnc-acceptance")]
    pub(crate) fn diagnostics() -> (bool, bool, [u32; 5]) {
        (
            ARMED.load(Ordering::Relaxed),
            VNC.load(Ordering::Relaxed),
            std::array::from_fn(|i| COUNTERS[i].load(Ordering::Relaxed)),
        )
    }

    /// Keystrokes the callback swallowed, waiting for the UI thread to forward
    /// them. A channel rather than a lock the callback might contend on: the
    /// callback never waits for UI or network work.
    type QueuedStroke = (u64, Entry<Stroke>);
    struct QueueReceiver {
        receiver: Receiver<QueuedStroke>,
        generation: u64,
        ordered: OrderedQueue<Stroke>,
    }
    static QUEUE: OnceLock<(Sender<QueuedStroke>, Mutex<QueueReceiver>)> = OnceLock::new();

    struct HookWorker {
        thread_id: u32,
        stop: Arc<AtomicBool>,
        alive: Arc<AtomicBool>,
    }
    struct HookState {
        worker: Option<HookWorker>,
        raw_keyboard: Option<KeyboardRegistrationLease>,
    }
    static HOOK: Mutex<HookState> = Mutex::new(HookState {
        worker: None,
        raw_keyboard: None,
    });

    #[cfg(feature = "native-vnc-acceptance")]
    pub(crate) fn worker_thread_id() -> Option<u32> {
        HOOK.lock()
            .ok()?
            .worker
            .as_ref()
            .map(|worker| worker.thread_id)
    }
    #[cfg(feature = "native-vnc-acceptance")]
    pub(crate) fn worker_health() -> (u32, u32) {
        (
            PUMP_TURNS.load(Ordering::Relaxed),
            PUMP_EXIT.load(Ordering::Relaxed),
        )
    }

    /// One swallowed key, already in the form the RDP wire wants.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct Stroke {
        pub(crate) scancode: u8,
        pub(crate) extended: bool,
        pub(crate) pressed: bool,
        // Standalone acceptance probes include the hook without routing VNC.
        #[cfg_attr(feature = "native-vnc-acceptance", allow(dead_code))]
        pub(crate) chord: Option<super::vnc_chord::Stroke>,
    }

    fn queue() -> &'static (Sender<QueuedStroke>, Mutex<QueueReceiver>) {
        QUEUE.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            (tx, Mutex::new(QueueReceiver { receiver: rx, generation: 0, ordered: OrderedQueue::default() }))
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

    /// The hook itself. Runs on its installing thread for **every** keystroke
    /// on the machine, so it does as little as is possible.
    ///
    /// # Safety
    /// Called by Windows with `lparam` pointing at a `KBDLLHOOKSTRUCT` whenever
    /// `code == HC_ACTION` (0).
    unsafe extern "system" fn callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        #[cfg(feature = "native-vnc-acceptance")]
        COUNTERS[0].fetch_add(1, Ordering::Relaxed);
        // Anything but HC_ACTION must be passed on untouched, per the API
        // contract — and the struct is only valid then.
        let generation = GENERATION.load(Ordering::Acquire);
        if code != 0
            || !ARMED.load(Ordering::Acquire)
            || WORKER_GENERATION.with(Cell::get) != generation
        {
            return CallNextHookEx(None, code, wparam, lparam);
        }
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        #[cfg(feature = "native-vnc-acceptance")]
        COUNTERS[1].fetch_add(1, Ordering::Relaxed);

        // Injected keys are left alone: `SendInput` from another tool would
        // otherwise loop back through here.
        const LLKHF_INJECTED: u32 = 0x10;
        if info.flags.0 & LLKHF_INJECTED != 0 {
            #[cfg(feature = "native-vnc-acceptance")]
            {
                COUNTERS[2].fetch_add(1, Ordering::Relaxed);
                if info.dwExtraInfo == probe_input_tag() && info.vkCode == 0x87 {
                    PROBE_EVENTS.fetch_add(1, Ordering::Relaxed);
                }
            }
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // WindowUnfocused is delivered through the UI loop. Do not wait for it
        // when that loop is stalled: keys for other applications remain local.
        let foreground = GetForegroundWindow();
        let mut process = 0;
        GetWindowThreadProcessId(foreground, Some(&mut process));
        if foreground.0 as isize != TARGET_WINDOW.load(Ordering::Acquire)
            || process != std::process::id()
        {
            CHORDS.with(|state| {
                state.borrow_mut().foreign_event(
                    info.vkCode as u16,
                    (info.scanCode & 0xff) as u8,
                    super::vnc_chord::physical_extended(info.vkCode as u16, info.flags.0 & 1 != 0),
                    matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN),
                )
            });
            WINDOW_MODIFIERS.with(|state| state.borrow_mut().fill(false));
            return CallNextHookEx(None, code, wparam, lparam);
        }

        let pressed = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let released = matches!(wparam.0 as u32, WM_KEYUP | WM_SYSKEYUP);
        if !pressed && !released {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // The way out of fullscreen has to keep working locally.
        const VK_RETURN: u32 = 0x0D;
        let owned_vnc_enter_up = info.vkCode == VK_RETURN
            && released
            && VNC.load(Ordering::Relaxed)
            && CHORDS.with(|state| {
                state
                    .borrow()
                    .member_down((info.scanCode & 0xff) as u8, info.flags.0 & 1 != 0)
            });
        // A Ctrl/Alt press after Win+Enter-down must not strand that owned
        // Enter. Only its key-up bypasses escape handling; new Escape downs
        // and the RDP contract keep their existing local priority.
        if info.vkCode == VK_RETURN && ctrl_and_alt_down() && !owned_vnc_enter_up {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // Ordinary text keeps its window/IME path. Once we swallow Win, its
        // chord members cannot use that path: IME no longer sees the logo and
        // would compose the shortcut letter locally. Capture its layout state
        // here and translate outside this timed callback instead.
        let mut chord = None;
        if VNC.load(Ordering::Relaxed) {
            use super::vnc_chord::{Modifiers, Route};
            use windows::Win32::UI::Input::KeyboardAndMouse::{
                GetAsyncKeyState, GetKeyboardLayout,
            };
            let scan = (info.scanCode & 0xff) as u8;
            let extended = super::vnc_chord::physical_extended(info.vkCode as u16, info.flags.0 & 1 != 0);
            let was_down = CHORDS.with(|state| state.borrow().key_down(scan, extended));
            let route = CHORDS.with(|state| {
                state
                    .borrow_mut()
                    .event(info.vkCode as u16, scan, extended, pressed)
            });
            if route == Route::Local {
                // These keys must remain visible to Windows/IME and the local
                // escape shortcut. Their window delivery is an ordering fence:
                // a tick must not replay a later Win/Tab before this edge.
                if matches!(info.vkCode, 0x10..=0x12 | 0xa0..=0xa5) {
                    let index = usize::from(scan) + if extended { 256 } else { 0 };
                    let fence = WINDOW_MODIFIERS.with(|state| {
                        let mut state = state.borrow_mut();
                        if pressed && !was_down {
                            state[index] = true;
                            true
                        } else if released && state[index] {
                            state[index] = false;
                            true
                        } else {
                            false
                        }
                    });
                    if fence {
                        let _ = queue().0.send((generation, Entry::WindowModifier(ModifierEdge {
                            scan, extended, pressed,
                        })));
                    }
                }
                return CallNextHookEx(None, code, wparam, lparam);
            }
            if let Route::Member { repeat } = route {
                let down = |vk: i32| (GetAsyncKeyState(vk) as u16 & 0x8000) != 0;
                let modifiers = CHORDS.with(|state| {
                    let state = state.borrow();
                    let left_shift = down(0xa0) || state.member_down(0x2a, false);
                    let right_shift = down(0xa1) || state.member_down(0x36, false);
                    let left_control = down(0xa2) || state.member_down(0x1d, false);
                    let right_control = down(0xa3) || state.member_down(0x1d, true);
                    let left_alt = down(0xa4) || state.member_down(0x38, false);
                    let right_alt = down(0xa5) || state.member_down(0x38, true);
                    Modifiers {
                        shift: left_shift || right_shift,
                        control: left_control || right_control,
                        alt: left_alt || right_alt,
                        right_alt,
                        caps: state.caps(),
                    }
                });
                let thread = GetWindowThreadProcessId(foreground, None);
                chord = Some(super::vnc_chord::Stroke {
                    vk: info.vkCode as u16,
                    scan,
                    extended,
                    pressed,
                    repeat,
                    layout: GetKeyboardLayout(thread).0 as isize,
                    modifiers,
                });
            }
            #[cfg(feature = "native-vnc-acceptance")]
            COUNTERS[3].fetch_add(1, Ordering::Relaxed);
        }
        const LLKHF_EXTENDED: u32 = 0x01;
        let stroke = Stroke {
            scancode: (info.scanCode & 0xFF) as u8,
            extended: info.flags.0 & LLKHF_EXTENDED != 0,
            pressed,
            chord,
        };
        // A failed send means the receiver is gone, i.e. we are shutting down;
        // passing the key on is the right thing then.
        if queue().0.send((generation, Entry::Key(stroke))).is_err() {
            CHORDS.with(|state| state.borrow_mut().clear());
            return CallNextHookEx(None, code, wparam, lparam);
        }
        #[cfg(feature = "native-vnc-acceptance")]
        COUNTERS[4].fetch_add(1, Ordering::Relaxed);
        // Swallowed: a non-zero return stops the key reaching hotkey dispatch
        // and every window below.
        LRESULT(1)
    }

    /// Install the hook if it is not already installed.
    pub(crate) fn arm_vnc() -> bool {
        arm_mode(true, unsafe { GetForegroundWindow() }.0 as isize)
    }

    /// Observe delivery without ever targeting a window for interception.
    #[cfg(feature = "native-vnc-acceptance")]
    pub(crate) fn arm_observer() -> bool {
        arm_mode(true, 0)
    }

    pub(crate) fn arm() -> bool {
        arm_mode(false, unsafe { GetForegroundWindow() }.0 as isize)
    }

    fn arm_mode(vnc: bool, target_window: isize) -> bool {
        let mut held = HOOK.lock().unwrap_or_else(|e| e.into_inner());
        if held
            .worker
            .as_ref()
            .is_some_and(|worker| worker.alive.load(Ordering::Acquire))
            && ARMED.load(Ordering::Acquire)
            && VNC.load(Ordering::Acquire) == vnc
            && TARGET_WINDOW.load(Ordering::Acquire) == target_window
        {
            // A window recreated while capture is active may register again.
            // An empty new lease must not discard the original registration.
            match KeyboardRegistrationLease::suspend() {
                Ok(lease) if lease.pending() => held.raw_keyboard = Some(lease),
                Ok(_) => {}
                Err(error) => {
                    ARMED.store(false, Ordering::Release);
                    eprintln!("keyboard capture raw input suspension failed: {error}");
                    return false;
                }
            }
            VNC.store(vnc, Ordering::Release);
            ARMED.store(true, Ordering::Release);
            return true;
        }
        ARMED.store(false, Ordering::Release);
        TARGET_WINDOW.store(target_window, Ordering::Release);
        if let Some(worker) = held.worker.take() {
            stop_worker(worker);
        }
        if !restore_raw_keyboard(&mut held) {
            return false;
        }
        // Our foreground winit window can receive WM_KEYDOWN while its raw
        // keyboard registration prevents WH_KEYBOARD_LL delivery (#210).
        // Only suspend this process's keyboard class during capture; window
        // keyboard/IME events and mouse raw input keep their normal routes.
        match KeyboardRegistrationLease::suspend() {
            Ok(lease) => held.raw_keyboard = Some(lease),
            Err(error) => {
                eprintln!("keyboard capture raw input suspension failed: {error}");
                return false;
            }
        }
        // Windows dispatches WH_KEYBOARD_LL to the installing thread. A UI
        // stall can silently remove it after LowLevelHooksTimeout; even pumping
        // the UI afterwards cannot recover that handle. Keep its pump isolated.
        let generation = GENERATION.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        let (preheld, caps) = unsafe {
            use windows::Win32::UI::Input::KeyboardAndMouse::{
                GetAsyncKeyState, GetKeyState, GetKeyboardLayout, MapVirtualKeyExW,
                MAPVK_VK_TO_VSC_EX,
            };
            let thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
            let layout = GetKeyboardLayout(thread);
            let caps = (thread == windows::Win32::System::Threading::GetCurrentThreadId())
                .then(|| GetKeyState(0x14) & 1 != 0);
            let mut down = [false; 512];
            for vk in 1..255 {
                // Generic modifier VKs map to the left scan even if only the
                // right physical key is held. Seed only their explicit halves.
                if matches!(vk, 0x10..=0x12) {
                    continue;
                }
                if GetAsyncKeyState(vk) as u16 & 0x8000 == 0 {
                    continue;
                }
                let scan = MapVirtualKeyExW(vk as u32, MAPVK_VK_TO_VSC_EX, Some(layout));
                if scan != 0 {
                    down[(scan & 0xff) as usize + if scan & 0xff00 != 0 { 256 } else { 0 }] = true;
                }
            }
            (down, caps)
        };
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let alive = Arc::new(AtomicBool::new(false));
        let worker_alive = Arc::clone(&alive);
        let _ = queue();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("adit-keyboard-hook".into())
            .spawn(move || {
                WORKER_GENERATION.with(|value| value.set(generation));
                CHORDS.with(|state| {
                    *state.borrow_mut() = super::vnc_chord::Ownership::new(preheld, caps)
                });
                WINDOW_MODIFIERS.with(|state| state.borrow_mut().fill(false));
                #[cfg(feature = "native-vnc-acceptance")]
                {
                    PUMP_TURNS.store(0, Ordering::Relaxed);
                    PUMP_EXIT.store(0, Ordering::Relaxed);
                }
                // Initialize both the TLS and channel before entering a timed
                // callback. PeekMessage also creates the thread's message queue.
                unsafe {
                    let mut message = MSG::default();
                    let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
                    let module = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                        .ok()
                        .map(|module| windows::Win32::Foundation::HINSTANCE(module.0));
                    #[cfg(all(test, feature = "native-vnc-acceptance"))]
                    if FAIL_NEXT_INSTALL.swap(false, Ordering::AcqRel) {
                        return;
                    }
                    let Ok(hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(callback), module, 0)
                    else {
                        return;
                    };
                    let thread_id = windows::Win32::System::Threading::GetCurrentThreadId();
                    let timer = SetTimer(None, 0, 50, None);
                    worker_alive.store(timer != 0, Ordering::Release);
                    if timer != 0 && ready_tx.send(thread_id).is_ok() {
                        while !worker_stop.load(Ordering::Acquire) {
                            let received = GetMessageW(&mut message, None, 0, 0).0;
                            #[cfg(feature = "native-vnc-acceptance")]
                            PUMP_TURNS.fetch_add(1, Ordering::Relaxed);
                            if received <= 0 {
                                #[cfg(feature = "native-vnc-acceptance")]
                                PUMP_EXIT
                                    .store(if received == 0 { 2 } else { 3 }, Ordering::Relaxed);
                                break;
                            }
                            let _ = TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                    }
                    if timer != 0 {
                        let _ = KillTimer(None, timer);
                    }
                    let _ = UnhookWindowsHookEx(hook);
                    worker_alive.store(false, Ordering::Release);
                    // Only the UI owner changes the shared input gate. An old
                    // worker checking its epoch then clearing ARMED could race
                    // a new activation and turn the new owner off.
                    #[cfg(feature = "native-vnc-acceptance")]
                    if worker_stop.load(Ordering::Acquire) {
                        PUMP_EXIT.store(1, Ordering::Relaxed);
                    }
                }
            });
        if spawned.is_ok() {
            if let Ok(thread_id) = ready_rx.recv_timeout(Duration::from_millis(250)) {
                if !alive.load(Ordering::Acquire) {
                    stop.store(true, Ordering::Release);
                    let _ = restore_raw_keyboard(&mut held);
                    return false;
                }
                VNC.store(vnc, Ordering::Release);
                ARMED.store(true, Ordering::Release);
                held.worker = Some(HookWorker {
                    thread_id,
                    stop,
                    alive,
                });
                return true;
            }
        }
        stop.store(true, Ordering::Release);
        ARMED.store(false, Ordering::Release);
        let _ = restore_raw_keyboard(&mut held);
        false
    }

    fn restore_raw_keyboard(state: &mut HookState) -> bool {
        if let Some(lease) = &mut state.raw_keyboard {
            if let Err(error) = lease.restore() {
                // Keep the lease so a later disarm can retry. A retiring worker
                // must never restore over the next activation's registration.
                eprintln!("keyboard capture raw input restore failed: {error}");
                return false;
            }
            if !lease.pending() {
                state.raw_keyboard = None;
            }
        }
        true
    }

    fn stop_worker(worker: HookWorker) {
        worker.stop.store(true, Ordering::Release);
        unsafe {
            let _ = PostThreadMessageW(worker.thread_id, WM_NULL, WPARAM(0), LPARAM(0));
        }
    }

    /// Remove the hook. Safe to call when nothing is installed.
    pub(crate) fn disarm() {
        ARMED.store(false, Ordering::Release);
        GENERATION.fetch_add(1, Ordering::AcqRel);
        let mut held = HOOK.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(worker) = held.worker.take() {
            // Do not join a global-hook thread from the UI: Windows can be
            // waiting for that same UI during CallNextHookEx. Epoch checks stop
            // capture synchronously; the owner unhooks as its pump exits.
            stop_worker(worker);
        }
        let _ = restore_raw_keyboard(&mut held);
        // Anything swallowed but not yet forwarded is dropped: it was meant for
        // a session no longer in the mode that captured it, and replaying it
        // afterwards would fire keys the user cannot see land.
        if let Ok(mut rx) = queue().1.lock() {
            while rx.receiver.try_recv().is_ok() {}
            rx.ordered = OrderedQueue::default();
        }
    }

    /// Take everything swallowed since the last call.
    pub(crate) fn drain() -> Vec<Stroke> {
        let Ok(mut rx) = queue().1.lock() else {
            return Vec::new();
        };
        receive_current(&mut rx);
        rx.ordered.take_all()
    }

    fn receive_current(rx: &mut QueueReceiver) {
        let generation = GENERATION.load(Ordering::Acquire);
        if rx.generation != generation {
            rx.ordered = OrderedQueue::default();
            rx.generation = generation;
        }
        while let Ok((owner, entry)) = rx.receiver.try_recv() {
            if owner == generation {
                rx.ordered.push(entry);
            }
        }
    }

    // Standalone native probes include this source without the VNC UI router.
    #[cfg_attr(feature = "native-vnc-acceptance", allow(dead_code))]
    pub(crate) fn drain_vnc(acknowledged: Option<ModifierEdge>) -> (Vec<Stroke>, bool) {
        let Ok(mut rx) = queue().1.lock() else { return (Vec::new(), true) };
        receive_current(&mut rx);
        rx.ordered.drain(acknowledged, std::time::Instant::now())
    }
}

#[cfg(not(windows))]
mod imp {
    #[cfg(feature = "native-vnc-acceptance")]
    pub(crate) fn probe_input_tag() -> usize {
        0
    }
    #[cfg(feature = "native-vnc-acceptance")]
    pub(crate) fn probe_events() -> u32 {
        0
    }
    #[cfg(feature = "native-vnc-acceptance")]
    pub(crate) fn arm_observer() -> bool {
        false
    }
    #[cfg(feature = "native-vnc-acceptance")]
    pub(crate) fn worker_thread_id() -> Option<u32> {
        None
    }
    #[cfg(feature = "native-vnc-acceptance")]
    pub(crate) fn worker_health() -> (u32, u32) {
        (0, 0)
    }
    #[cfg(feature = "native-vnc-acceptance")]
    pub(crate) fn diagnostics() -> (bool, bool, [u32; 5]) {
        (false, false, [0; 5])
    }
    /// The hook is a Win32 mechanism and RDP ships on Windows only, so the other
    /// platforms get a shape that compiles and does nothing.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct Stroke {
        pub(crate) scancode: u8,
        pub(crate) extended: bool,
        pub(crate) pressed: bool,
        #[cfg_attr(feature = "native-vnc-acceptance", allow(dead_code))]
        pub(crate) chord: Option<super::vnc_chord::Stroke>,
    }

    pub(crate) fn arm() -> bool {
        false
    }
    pub(crate) fn arm_vnc() -> bool {
        false
    }
    pub(crate) fn disarm() {}
    pub(crate) fn drain() -> Vec<Stroke> {
        Vec::new()
    }
    pub(crate) fn drain_vnc(_: Option<super::vnc_order::ModifierEdge>) -> (Vec<Stroke>, bool) {
        (Vec::new(), false)
    }
}

// `Stroke` stays inside the module: callers read its fields off whatever
// `drain` hands back and never name the type.
#[cfg(feature = "native-vnc-acceptance")]
pub(crate) use imp::arm_observer;
#[cfg(feature = "native-vnc-acceptance")]
pub(crate) use imp::diagnostics;
#[cfg(feature = "native-vnc-acceptance")]
pub(crate) use imp::worker_health;
#[cfg(feature = "native-vnc-acceptance")]
pub(crate) use imp::worker_thread_id;
pub(crate) use imp::{arm, arm_vnc, disarm, drain};
#[cfg_attr(feature = "native-vnc-acceptance", allow(unused_imports))]
pub(crate) use imp::drain_vnc;
#[cfg(feature = "native-vnc-acceptance")]
pub(crate) use imp::{probe_events, probe_input_tag};

#[cfg(all(test, windows, feature = "native-vnc-acceptance"))]
mod native_tests {
    use super::*;
    use std::time::{Duration, Instant};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_F24,
    };
    use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

    #[test]
    #[ignore = "changes only this test process's raw keyboard registration"]
    fn hook_install_failure_restores_raw_keyboard_registration() {
        use super::raw_input::KeyboardRegistrationLease;
        use windows::Win32::UI::Input::*;
        fn devices() -> Vec<RAWINPUTDEVICE> {
            let mut data = vec![RAWINPUTDEVICE::default(); 64];
            let mut count = data.len() as u32;
            let got = unsafe {
                GetRegisteredRawInputDevices(
                    Some(data.as_mut_ptr()),
                    &mut count,
                    size_of::<RAWINPUTDEVICE>() as u32,
                )
            };
            assert_ne!(got, u32::MAX);
            data.truncate(got as usize);
            data.sort_by_key(|d| (d.usUsagePage, d.usUsage));
            data
        }
        struct Restore(KeyboardRegistrationLease);
        impl Drop for Restore {
            fn drop(&mut self) {
                disarm();
                unsafe {
                    let _ = RegisterRawInputDevices(
                        &[RAWINPUTDEVICE {
                            usUsagePage: 1,
                            usUsage: 6,
                            dwFlags: RIDEV_REMOVE,
                            ..Default::default()
                        }],
                        size_of::<RAWINPUTDEVICE>() as u32,
                    );
                }
                let _ = self.0.restore();
            }
        }
        let original = devices();
        let restore = Restore(KeyboardRegistrationLease::suspend().unwrap());
        unsafe {
            RegisterRawInputDevices(
                &[RAWINPUTDEVICE {
                    usUsagePage: 1,
                    usUsage: 6,
                    dwFlags: RIDEV_DEVNOTIFY,
                    ..Default::default()
                }],
                size_of::<RAWINPUTDEVICE>() as u32,
            )
            .unwrap();
        }
        let baseline = devices();
        super::imp::FAIL_NEXT_INSTALL.store(true, std::sync::atomic::Ordering::Release);
        assert!(
            !arm_observer(),
            "forced hook installation failure reported success"
        );
        assert!(!diagnostics().0);
        assert_eq!(
            devices(),
            baseline,
            "failed arm lost raw input registration"
        );
        disarm();
        assert_eq!(
            devices(),
            baseline,
            "repeat disarm changed restored registration"
        );
        drop(restore);
        assert_eq!(
            devices(),
            original,
            "test did not restore its original registration"
        );
    }

    #[test]
    #[ignore = "injects only F24 on an explicitly selected Windows test desktop"]
    fn hook_delivery_survives_a_stalled_ui_thread() {
        // This proves real callback delivery, not interception of a physical
        // shortcut: shipping code deliberately passes injected keys through.
        arm_vnc();
        assert!(diagnostics().0, "hook installation failed");
        let before = diagnostics().2;
        let injector = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            let inputs = [false, true].map(|up| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_F24,
                        dwFlags: if up {
                            KEYEVENTF_KEYUP
                        } else {
                            Default::default()
                        },
                        ..Default::default()
                    },
                },
            });
            assert_eq!(unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) }, 2);
        });
        // Longer than Windows' maximum LowLevelHooksTimeout. Rendering or an
        // unrelated OS call on the UI thread must not destroy system capture.
        std::thread::sleep(Duration::from_millis(1600));
        let until = Instant::now() + Duration::from_millis(200);
        while Instant::now() < until {
            let mut message = MSG::default();
            unsafe {
                let _ = PeekMessageW(&mut message, None, 0, 0, PM_REMOVE);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        injector.join().unwrap();
        let after = diagnostics().2;
        disarm();
        println!("native hook delivery: before={before:?}, after={after:?}");
        assert!(
            after[0].wrapping_sub(before[0]) >= 2,
            "callback was not delivered during UI stall"
        );
        assert!(
            after[2].wrapping_sub(before[2]) >= 2,
            "injected input must remain local"
        );
        assert_eq!(after[4], before[4], "injected input was swallowed");
    }
}
