//! Temporarily release this application's raw keyboard registration during grab.
//!
//! The UI owns this lease: a retiring hook worker must never restore a snapshot
//! over the registration belonging to a newer grab or a recreated window.

use windows::core::{Error, Result, HRESULT};
use windows::Win32::Foundation::{
    GetLastError, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_DATA, ERROR_INVALID_WINDOW_HANDLE, HWND,
    WIN32_ERROR,
};
use windows::Win32::UI::Input::{
    GetRegisteredRawInputDevices, RegisterRawInputDevices, RAWINPUTDEVICE, RAWINPUTDEVICE_FLAGS,
    RIDEV_REMOVE,
};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

const KEYBOARD_PAGE: u16 = 1;
const KEYBOARD_USAGE: u16 = 6;

#[derive(Debug, Clone, Copy)]
struct KeyboardRegistration {
    flags: u32,
    target: isize,
    target_thread: u32,
}

#[derive(Debug)]
pub(crate) struct KeyboardRegistrationLease {
    saved: Option<KeyboardRegistration>,
}

impl KeyboardRegistrationLease {
    /// Called on the UI control path, before enabling the hook's input gate.
    pub(crate) fn suspend() -> Result<Self> {
        let Some(original) = keyboard_registration()? else {
            return Ok(Self { saved: None });
        };
        let target = original.hwndTarget.0 as isize;
        let target_thread = if target == 0 {
            0
        } else {
            let mut process = 0;
            let thread =
                unsafe { GetWindowThreadProcessId(original.hwndTarget, Some(&mut process)) };
            if thread == 0 || process != std::process::id() {
                return Err(win32_error(ERROR_INVALID_WINDOW_HANDLE));
            }
            thread
        };
        let remove = RAWINPUTDEVICE {
            usUsagePage: KEYBOARD_PAGE,
            usUsage: KEYBOARD_USAGE,
            dwFlags: RIDEV_REMOVE,
            // RIDEV_REMOVE rejects non-null targets, even when the saved
            // registration had a target for background device notifications.
            hwndTarget: HWND::default(),
        };
        unsafe { RegisterRawInputDevices(&[remove], size_of::<RAWINPUTDEVICE>() as u32) }?;
        Ok(Self {
            saved: Some(KeyboardRegistration {
                flags: original.dwFlags.0,
                target,
                target_thread,
            }),
        })
    }

    /// Restore once; a failed API call leaves the snapshot available for retry.
    pub(crate) fn restore(&mut self) -> Result<()> {
        let Some(saved) = self.saved else {
            return Ok(());
        };
        // Raw Input registration is process-wide per device class. Preserve a
        // later registrant, including a new event-loop window, instead of
        // overwriting it with a stale snapshot.
        if keyboard_registration()?.is_some() {
            self.saved = None;
            return Ok(());
        }
        let target = HWND(saved.target as *mut core::ffi::c_void);
        if saved.target != 0 {
            let mut process = 0;
            let thread = unsafe { GetWindowThreadProcessId(target, Some(&mut process)) };
            // HWND values can be recycled. A destroyed target, or a different
            // process/thread now owning its value, must not be re-registered.
            if thread == 0 || thread != saved.target_thread || process != std::process::id() {
                self.saved = None;
                return Ok(());
            }
        }
        let original = RAWINPUTDEVICE {
            usUsagePage: KEYBOARD_PAGE,
            usUsage: KEYBOARD_USAGE,
            dwFlags: RAWINPUTDEVICE_FLAGS(saved.flags),
            hwndTarget: target,
        };
        unsafe { RegisterRawInputDevices(&[original], size_of::<RAWINPUTDEVICE>() as u32) }?;
        self.saved = None;
        Ok(())
    }

    pub(crate) fn pending(&self) -> bool {
        self.saved.is_some()
    }
}

fn keyboard_registration() -> Result<Option<RAWINPUTDEVICE>> {
    let mut capacity = 8usize;
    for _ in 0..3 {
        let mut devices = vec![RAWINPUTDEVICE::default(); capacity];
        let mut count = capacity as u32;
        let written = unsafe {
            GetRegisteredRawInputDevices(
                Some(devices.as_mut_ptr()),
                &mut count,
                size_of::<RAWINPUTDEVICE>() as u32,
            )
        };
        if written != u32::MAX {
            if written as usize > devices.len() {
                return Err(win32_error(ERROR_INVALID_DATA));
            }
            devices.truncate(written as usize);
            return Ok(devices.into_iter().find(|device| {
                device.usUsagePage == KEYBOARD_PAGE && device.usUsage == KEYBOARD_USAGE
            }));
        }
        let error = unsafe { GetLastError() };
        if error != ERROR_INSUFFICIENT_BUFFER {
            return Err(win32_error(error));
        }
        // Another component can register devices between reads. Retry a bounded
        // number of times without trusting an unbounded allocation request.
        if count <= capacity as u32 || count > 4096 {
            return Err(win32_error(ERROR_INVALID_DATA));
        }
        capacity = count as usize;
    }
    Err(win32_error(ERROR_INSUFFICIENT_BUFFER))
}

fn win32_error(error: WIN32_ERROR) -> Error {
    Error::from_hresult(HRESULT::from_win32(error.0))
}

#[cfg(all(test, feature = "native-vnc-acceptance"))]
mod native_tests {
    use super::*;
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP,
    };

    // No visible windows, activation or injected input. Distinct owned targets
    // identify the later registrant even if Windows normalizes some raw flags.
    struct HiddenWindow(HWND);

    impl HiddenWindow {
        fn new() -> Result<Self> {
            unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    w!(""),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    None,
                    None,
                    None,
                    None,
                )
                .map(Self)
            }
        }
    }

    impl Drop for HiddenWindow {
        fn drop(&mut self) {
            if let Err(error) = unsafe { DestroyWindow(self.0) } {
                eprintln!("raw lease test hidden window cleanup failed: {error}");
            }
        }
    }

    struct OriginalKeyboardGuard {
        original: Option<RAWINPUTDEVICE>,
        restored: bool,
    }

    impl OriginalKeyboardGuard {
        fn restore(&mut self) -> Result<()> {
            if self.restored {
                return Ok(());
            }
            set_keyboard(None)?;
            if let Some(original) = self.original {
                set_keyboard(Some(original))?;
            }
            self.restored = true;
            Ok(())
        }
    }

    impl Drop for OriginalKeyboardGuard {
        fn drop(&mut self) {
            if let Err(error) = self.restore() {
                eprintln!("raw lease test original keyboard cleanup failed: {error}");
            }
        }
    }

    fn set_keyboard(registration: Option<RAWINPUTDEVICE>) -> Result<()> {
        let registration = registration.unwrap_or(RAWINPUTDEVICE {
            usUsagePage: KEYBOARD_PAGE,
            usUsage: KEYBOARD_USAGE,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        });
        unsafe { RegisterRawInputDevices(&[registration], size_of::<RAWINPUTDEVICE>() as u32) }
    }

    fn registrations() -> Result<Vec<RAWINPUTDEVICE>> {
        // An independent full-list read also verifies mouse and all other
        // device classes, rather than trusting the lease's keyboard lookup.
        let mut devices = vec![RAWINPUTDEVICE::default(); 4096];
        let mut count = devices.len() as u32;
        let written = unsafe {
            GetRegisteredRawInputDevices(
                Some(devices.as_mut_ptr()),
                &mut count,
                size_of::<RAWINPUTDEVICE>() as u32,
            )
        };
        if written == u32::MAX {
            return Err(win32_error(unsafe { GetLastError() }));
        }
        assert!(written as usize <= devices.len());
        devices.truncate(written as usize);
        Ok(devices)
    }

    fn signature(
        devices: &[RAWINPUTDEVICE],
        non_keyboard_only: bool,
    ) -> Vec<(u16, u16, u32, isize)> {
        let mut result: Vec<_> = devices
            .iter()
            .filter(|device| {
                !non_keyboard_only
                    || device.usUsagePage != KEYBOARD_PAGE
                    || device.usUsage != KEYBOARD_USAGE
            })
            .map(|device| {
                (
                    device.usUsagePage,
                    device.usUsage,
                    device.dwFlags.0,
                    device.hwndTarget.0 as isize,
                )
            })
            .collect();
        result.sort_unstable();
        result
    }

    #[test]
    #[ignore = "temporarily changes only this isolated test process's raw keyboard registration"]
    fn raw_keyboard_lease_restores_and_preserves_later_registrants() -> Result<()> {
        let baseline = registrations()?;
        let mut original_guard = OriginalKeyboardGuard {
            original: keyboard_registration()?,
            restored: false,
        };
        let expected_other_devices = signature(&baseline, true);
        let assert_other_devices = || -> Result<()> {
            assert_eq!(signature(&registrations()?, true), expected_other_devices);
            Ok(())
        };

        set_keyboard(None)?;
        assert!(keyboard_registration()?.is_none());
        let mut empty = KeyboardRegistrationLease::suspend()?;
        assert!(!empty.pending());
        empty.restore()?;
        empty.restore()?;
        assert!(keyboard_registration()?.is_none());
        assert_other_devices()?;

        let first_window = HiddenWindow::new()?;
        let second_window = HiddenWindow::new()?;
        let registration = |target| RAWINPUTDEVICE {
            usUsagePage: KEYBOARD_PAGE,
            usUsage: KEYBOARD_USAGE,
            dwFlags: RAWINPUTDEVICE_FLAGS::default(),
            hwndTarget: target,
        };
        set_keyboard(Some(registration(first_window.0)))?;
        let first = keyboard_registration()?.expect("first keyboard registration exists");
        assert_eq!(first.hwndTarget, first_window.0);
        let mut lease = KeyboardRegistrationLease::suspend()?;
        assert!(lease.pending());
        assert!(keyboard_registration()?.is_none());
        assert_other_devices()?;
        lease.restore()?;
        assert!(!lease.pending());
        assert_eq!(keyboard_registration()?, Some(first));
        lease.restore()?;
        assert_eq!(keyboard_registration()?, Some(first));
        assert_other_devices()?;

        let mut superseded = KeyboardRegistrationLease::suspend()?;
        assert!(superseded.pending());
        assert!(keyboard_registration()?.is_none());
        set_keyboard(Some(registration(second_window.0)))?;
        let later = keyboard_registration()?.expect("later keyboard registration exists");
        assert_eq!(later.hwndTarget, second_window.0);
        assert_ne!(later.hwndTarget, first.hwndTarget);
        superseded.restore()?;
        assert!(!superseded.pending());
        assert_eq!(keyboard_registration()?, Some(later));
        superseded.restore()?;
        assert_eq!(keyboard_registration()?, Some(later));
        assert_other_devices()?;

        original_guard.restore()?;
        assert_eq!(
            signature(&registrations()?, false),
            signature(&baseline, false)
        );
        println!("raw keyboard lease native: absent, exact restore, later registrant preserved; non-keyboard classes unchanged");
        Ok(())
    }
}
