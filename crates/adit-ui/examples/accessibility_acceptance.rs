//! Requires ADIT_CONFIG_DIR to name a fresh empty directory beneath OS temp.
//! Opens only a synthetic iced window and uses Windows UIA, not injected clicks.
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    adit_ui::run_accessibility_acceptance()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows UIA acceptance is Windows-only");
    std::process::exit(2);
}
