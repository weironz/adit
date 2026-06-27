fn main() {
    // Embed the application icon as a Windows resource so Explorer, the
    // taskbar, and the Start-menu shortcut show it. Non-fatal: if no resource
    // compiler is available the build still succeeds, and the runtime window
    // icon (set in adit-ui) still applies.
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(error) = res.compile() {
            println!("cargo:warning=failed to embed icon resource: {error}");
        }
    }
}
