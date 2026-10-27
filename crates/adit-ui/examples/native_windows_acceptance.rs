//! Real ConPTY sessions; optionally add the repository's loopback RFB fixture.
//! Requires a fresh ADIT_CONFIG_DIR under the OS temp directory.
fn main() -> iced::Result {
    adit_ui::run_native_windows_acceptance()
}
