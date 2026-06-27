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
    let start_menu_dir = start_menu_dir()?;
    fs::create_dir_all(&start_menu_dir)?;
    let shortcut = start_menu_dir.join("Adit.lnk");

    let script = format!(
        "$shell = New-Object -ComObject WScript.Shell; \
         $shortcut = $shell.CreateShortcut('{}'); \
         $shortcut.TargetPath = '{}'; \
         $shortcut.WorkingDirectory = '{}'; \
         $shortcut.IconLocation = '{},0'; \
         $shortcut.Save()",
        ps_escape(&shortcut),
        ps_escape(app_path),
        ps_escape(install_dir),
        ps_escape(app_path),
    );

    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script.as_str(),
        ])
        .status();

    match status {
        Ok(status) if status.success() => Ok(()),
        _ => write_cmd_shortcut(&start_menu_dir, app_path),
    }
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
             rmdir /S /Q \"{}\"\r\n",
            install_dir.display()
        ),
    )
}

fn ps_escape(path: &Path) -> String {
    path.display().to_string().replace('\'', "''")
}
