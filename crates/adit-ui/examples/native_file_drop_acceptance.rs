#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    adit_ui::run_native_file_drop_acceptance()
}

#[cfg(not(windows))]
fn main() {}
