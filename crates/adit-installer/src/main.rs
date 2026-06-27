use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

const APP_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "\\adit-app.exe"));
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    if let Err(error) = install() {
        eprintln!("Adit installer failed: {error}");
        std::process::exit(1);
    }
}

fn install() -> io::Result<()> {
    if APP_BYTES.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "installer payload is empty; rebuild with ADIT_APP_EXE pointing to adit-app.exe",
        ));
    }

    let install_dir = install_dir()?;
    fs::create_dir_all(&install_dir)?;

    let app_path = install_dir.join("Adit.exe");
    fs::write(&app_path, APP_BYTES)?;
    write_uninstall_cmd(&install_dir)?;
    create_shortcut(&install_dir, &app_path)?;

    println!("Adit {APP_VERSION} installed successfully.");
    println!("Install location: {}", install_dir.display());
    println!("Start menu entry: Adit");

    Ok(())
}

fn install_dir() -> io::Result<PathBuf> {
    let Some(local_app_data) = env::var_os("LOCALAPPDATA") else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "LOCALAPPDATA is not set",
        ));
    };

    Ok(PathBuf::from(local_app_data).join("Programs").join("Adit"))
}

fn start_menu_dir() -> io::Result<PathBuf> {
    let Some(app_data) = env::var_os("APPDATA") else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "APPDATA is not set",
        ));
    };

    Ok(PathBuf::from(app_data)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Adit"))
}

fn create_shortcut(install_dir: &Path, app_path: &Path) -> io::Result<()> {
    // Start menu entry.
    let start_menu_dir = start_menu_dir()?;
    fs::create_dir_all(&start_menu_dir)?;
    let start_lnk = start_menu_dir.join("Adit.lnk");
    if !make_lnk(&start_lnk, app_path, install_dir) {
        write_cmd_shortcut(&start_menu_dir, app_path)?;
    }

    // Desktop shortcut (best-effort; resolves the real Desktop folder so it
    // works even when Desktop is redirected to OneDrive).
    create_desktop_shortcut(app_path, install_dir);

    Ok(())
}

/// Create a `.lnk` at a known path via the WScript.Shell COM object.
fn make_lnk(lnk: &Path, app_path: &Path, working_dir: &Path) -> bool {
    let script = format!(
        "$shell = New-Object -ComObject WScript.Shell; \
         $shortcut = $shell.CreateShortcut('{}'); \
         $shortcut.TargetPath = '{}'; \
         $shortcut.WorkingDirectory = '{}'; \
         $shortcut.IconLocation = '{},0'; \
         $shortcut.Save()",
        ps_escape(lnk),
        ps_escape(app_path),
        ps_escape(working_dir),
        ps_escape(app_path),
    );
    run_powershell(&script)
}

/// Create (or overwrite) the desktop shortcut, resolving the Desktop folder in
/// PowerShell to honour OneDrive/known-folder redirection.
fn create_desktop_shortcut(app_path: &Path, working_dir: &Path) {
    let script = format!(
        "$desktop = [Environment]::GetFolderPath('Desktop'); \
         $shell = New-Object -ComObject WScript.Shell; \
         $shortcut = $shell.CreateShortcut((Join-Path $desktop 'Adit.lnk')); \
         $shortcut.TargetPath = '{}'; \
         $shortcut.WorkingDirectory = '{}'; \
         $shortcut.IconLocation = '{},0'; \
         $shortcut.Save()",
        ps_escape(app_path),
        ps_escape(working_dir),
        ps_escape(app_path),
    );
    let _ = run_powershell(&script);
}

fn run_powershell(script: &str) -> bool {
    matches!(
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                script,
            ])
            .status(),
        Ok(status) if status.success()
    )
}

fn write_cmd_shortcut(start_menu_dir: &Path, app_path: &Path) -> io::Result<()> {
    let cmd = start_menu_dir.join("Adit.cmd");
    fs::write(
        cmd,
        format!("@echo off\r\nstart \"Adit\" \"{}\"\r\n", app_path.display()),
    )
}

fn write_uninstall_cmd(install_dir: &Path) -> io::Result<()> {
    let uninstall = install_dir.join("Uninstall Adit.cmd");
    fs::write(
        uninstall,
        format!(
            "@echo off\r\n\
             taskkill /IM Adit.exe /F >nul 2>nul\r\n\
             del \"%USERPROFILE%\\Desktop\\Adit.lnk\" >nul 2>nul\r\n\
             rmdir /S /Q \"%APPDATA%\\Microsoft\\Windows\\Start Menu\\Programs\\Adit\" >nul 2>nul\r\n\
             rmdir /S /Q \"{}\"\r\n",
            install_dir.display()
        ),
    )
}

fn ps_escape(path: &Path) -> String {
    path.display().to_string().replace('\'', "''")
}
