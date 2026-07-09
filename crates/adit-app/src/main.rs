// Release builds are GUI apps — suppress the console window that would otherwise
// flash on launch. Debug builds keep the console so logs remain visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> iced::Result {
    // Hold a named mutex for the whole process so the installer (Inno Setup's
    // `AppMutex`) can detect a running instance and ask the user to close it
    // cleanly, instead of the Restart Manager failing to auto-close the window
    // while it tries to replace Adit.exe.
    #[cfg(windows)]
    hold_instance_mutex();

    adit_ui::run()
}

/// Create a process-lifetime named mutex used only for running-instance
/// detection by the installer. The handle is intentionally leaked — Windows
/// releases the mutex automatically when the process exits.
#[cfg(windows)]
fn hold_instance_mutex() {
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = "AditAppInstanceMutex"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `name` is a valid NUL-terminated UTF-16 string; a null attributes
    // pointer with no initial ownership is well-defined. The returned handle is
    // deliberately dropped (leaked) so the mutex lives for the whole process.
    unsafe {
        let _ = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
    }
}
