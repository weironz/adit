// Release builds are GUI apps — suppress the console window that would otherwise
// flash on launch. Debug builds keep the console so logs remain visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> iced::Result {
    install_panic_hook();
    adit_ui::run()
}

/// Write panics to a crash log in the config folder. Release builds are
/// GUI-subsystem (no console), so a panic would otherwise vanish without a
/// trace; this leaves the message, location, backtrace, and version on disk.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let report = format!(
            "\n==== Adit {} panic @ unix {secs} ====\n{info}\n\nbacktrace:\n{backtrace}\n",
            env!("CARGO_PKG_VERSION"),
        );

        let dir = adit_storage::config_dir();
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("crash.log"))
        {
            use std::io::Write;
            let _ = file.write_all(report.as_bytes());
        }

        previous(info);
    }));
}
