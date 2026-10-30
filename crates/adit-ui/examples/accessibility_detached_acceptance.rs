//! Two real synthetic windows and UIA clients; no network or real user data.
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    adit_ui::run_accessibility_detached_acceptance()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows UIA acceptance is Windows-only");
}
