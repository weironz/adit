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

    // Enlarge the main-thread stack (MSVC only). iced runs its recursive layout
    // + draw and the wgpu pipeline on the main thread, which on Windows reserves
    // only 1 MiB by default. A deep widget tree (notably rendering the RDP image
    // surface) can spike past that and trip STATUS_STACK_OVERFLOW (0xC00000FD) —
    // which aborts the whole process instantly, before the panic hook can even
    // run (so nothing lands in crash.log). Reserve 32 MiB instead. This is
    // virtual address space, committed lazily, so it costs no real memory until
    // used. Validated: a 1 MiB stack overflows a ~1.5 MiB deep call; 32 MiB
    // survives it.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:33554432");
    }
}
