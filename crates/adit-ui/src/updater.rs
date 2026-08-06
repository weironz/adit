use super::*;

/// Kick off an update check: show the dialog in the "checking" state and query
/// GitHub in the background.
pub(crate) fn begin_update_check(app: &mut AditApp) -> Task<Message> {
    app.active_menu = None;
    app.update_dialog_open = true;
    app.update_state = UpdateState::Checking;
    Task::perform(check_for_update(), Message::UpdateChecked)
}

/// Hand a URL to the OS's default browser, receiving it as a single argv so no
/// shell ever re-parses it.
///
/// The Windows arm used to be `cmd /C start "" <url>`, and that silently
/// truncates any URL at its first `&`: Rust quotes an argument only when it
/// contains whitespace, so a percent-encoded URL reaches cmd bare and cmd reads
/// `&` as a command separator. An OAuth authorize URL is nothing *but*
/// `&`-joined parameters, so Dropbox received `...authorize?response_type=code`
/// and answered "Missing client_id" — a failure that reads like a missing
/// build-time id rather than a mangled URL. `%VAR%` expansion would have been
/// the same trap one step further along, since percent-encoding is full of `%`.
fn spawn_browser(url: &str) -> std::io::Result<std::process::Child> {
    if cfg!(target_os = "windows") {
        // rundll32 receives the URL as a single argv — no cmd.exe re-parsing.
        no_window(
            std::process::Command::new("rundll32.exe")
                .args(["url.dll,FileProtocolHandler", url]),
        )
        .spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    }
}

/// Open a URL in the default browser (best-effort). For URLs Adit itself built;
/// anything derived from remote output goes through `open_external_link`.
pub(crate) fn open_url(app: &mut AditApp, url: &str) {
    if let Err(error) = spawn_browser(url) {
        app.last_error = Some(tf("打开链接失败: {}", &[&error]));
    }
}

/// Whether `url` is an `http(s)` link Adit will open from terminal output. The
/// output is remote-controlled, so this is deliberately strict: anything but an
/// `http(s)` scheme (e.g. `file:`, `javascript:`) is refused, and **every char
/// must be printable ASCII**. That last rule rejects not just control/space
/// chars (a shell/arg-splitting vector) but all non-ASCII — including Unicode
/// bidi/format/separator characters (RLO, isolates, `U+2028`…) that could
/// visually reorder the URL shown in the confirmation dialog to spoof its real
/// destination. A legitimate URL is ASCII (non-ASCII is percent-encoded).
pub(crate) fn is_openable_http_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && url.len() <= 4096
        && url.chars().all(|c| c.is_ascii_graphic())
}

/// Open a terminal hyperlink in the OS browser WITHOUT going through a shell, so
/// a hostile URL can't inject a command. Only `http(s)` is allowed. The caller is
/// expected to have shown the user the destination and gotten confirmation first.
pub(crate) fn open_external_link(app: &mut AditApp, url: &str) {
    if !is_openable_http_url(url) {
        app.last_error = Some(String::from(t("仅支持打开 http/https 链接")));
        return;
    }
    if let Err(error) = spawn_browser(url) {
        app.last_error = Some(tf("打开链接失败: {}", &[&error]));
    }
}

/// Suppress the console window when spawning a console tool from the GUI app.
pub(crate) fn no_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

pub(crate) const UPDATE_REPO: &str = "weironz/adit";

/// Check GitHub for a newer release. `Ok(None)` = already up to date.
pub(crate) async fn check_for_update() -> Result<Option<UpdateInfo>, String> {
    tokio::task::spawn_blocking(check_for_update_blocking)
        .await
        .map_err(|error| tf("更新检查任务失败: {}", &[&error]))?
}

pub(crate) fn check_for_update_blocking() -> Result<Option<UpdateInfo>, String> {
    let url = format!("https://api.github.com/repos/{UPDATE_REPO}/releases/latest");
    let output = no_window(std::process::Command::new("curl").args([
        "-sSL",
        "--max-time",
        "25",
        "-H",
        "User-Agent: adit-updater",
        "-H",
        "Accept: application/vnd.github+json",
        &url,
    ]))
    .output()
    .map_err(|error| tf("无法运行 curl（检查更新需要系统自带的 curl）: {}", &[&error]))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(tf("检查更新失败: {}", &[&stderr.trim()]));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| tf("解析发布信息失败: {}", &[&error]))?;

    let tag = json["tag_name"]
        .as_str()
        .ok_or("发布信息缺少 tag_name")?
        .to_string();
    let notes_url = json["html_url"].as_str().unwrap_or_default().to_string();

    let current = env!("CARGO_PKG_VERSION");
    if !version_is_newer(&tag, current) {
        return Ok(None);
    }

    let (installer_url, installer_name) = json["assets"]
        .as_array()
        .and_then(|assets| pick_installer_asset(assets, std::env::consts::ARCH))
        .unwrap_or_default();

    Ok(Some(UpdateInfo {
        tag,
        installer_url,
        installer_name,
        notes_url,
    }))
}

/// Build the command to run the downloaded installer as a silent background
/// update: no wizard, installed in place over the current location, then the
/// installer relaunches Adit. A UAC prompt still appears for an all-users
/// (Program Files) install.
pub(crate) fn launch_silent_update(installer_path: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(installer_path);
    cmd.args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"]);

    // Update in place at the current install directory + scope, so a background
    // update never creates a second copy elsewhere.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cmd.arg(format!("/DIR={}", dir.display()));
            let in_program_files = dir
                .to_string_lossy()
                .to_lowercase()
                .contains("program files");
            cmd.arg(if in_program_files {
                "/ALLUSERS"
            } else {
                "/CURRENTUSER"
            });
        }
    }
    cmd
}

/// Download the installer to a temp folder; returns the saved path.
pub(crate) async fn download_installer(url: String, name: String) -> Result<String, String> {
    if url.is_empty() {
        return Err(String::from(t("该版本没有可下载的 Windows 安装包")));
    }
    tokio::task::spawn_blocking(move || download_installer_blocking(&url, &name))
        .await
        .map_err(|error| tf("下载任务失败: {}", &[&error]))?
}

pub(crate) fn download_installer_blocking(url: &str, name: &str) -> Result<String, String> {
    let dir = std::env::temp_dir().join("adit-update");
    std::fs::create_dir_all(&dir).map_err(|error| tf("创建下载目录失败: {}", &[&error]))?;
    let safe_name = if name.is_empty() { "adit-installer.exe" } else { name };
    let dest = dir.join(safe_name);

    let output = no_window(std::process::Command::new("curl").args([
        "-sSL",
        "--max-time",
        "600",
        "-H",
        "User-Agent: adit-updater",
        "-o",
        &dest.to_string_lossy(),
        url,
    ]))
    .output()
    .map_err(|error| tf("无法运行 curl: {}", &[&error]))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(tf("下载安装包失败: {}", &[&stderr.trim()]));
    }

    match std::fs::metadata(&dest) {
        Ok(meta) if meta.len() >= 200_000 => Ok(dest.to_string_lossy().to_string()),
        Ok(_) => Err(String::from(t("下载的安装包不完整，请重试"))),
        Err(error) => Err(tf("找不到下载的安装包: {}", &[&error])),
    }
}

/// Compare a `vX.Y.Z` (or `X.Y.Z`) tag against the current version.
/// Pick the Windows installer matching `arch` (as in [`std::env::consts::ARCH`]),
/// returning `(download_url, file_name)`.
///
/// Releases carry two installers now, and they are not interchangeable: handing
/// an x86_64 machine the arm64 build leaves it with nothing that runs at all.
/// Matching on the name is enough because only the arm64 asset carries an
/// architecture in its name — the x64 installer deliberately kept the name it has
/// always had, so that updaters older than this function, which took the first
/// `.exe` in the list, keep resolving to the build they are already running.
///
/// What holds that ordering up is the underscore in `_arm64`, not upload order:
/// GitHub returns a release's assets sorted by name. v0.1.61 shipped the arm64
/// installer as `-arm64.exe`, and since `-` sorts before `.` it came first, so
/// every old updater on an x64 machine was offered a build that refuses to run
/// there. `_` sorts after `.`. `installer_asset_ordering_favours_x64` pins it.
///
/// Falls back to any `.exe` rather than refusing to update, so a future release
/// that renames things degrades to the old behaviour instead of stranding
/// everyone on their current version.
pub(crate) fn pick_installer_asset(assets: &[serde_json::Value], arch: &str) -> Option<(String, String)> {
    let want_arm = arch == "aarch64";
    let mut fallback: Option<(String, String)> = None;

    for asset in assets {
        let Some(name) = asset["name"].as_str() else {
            continue;
        };
        if !name.ends_with(".exe") {
            continue;
        }
        let found = (
            asset["browser_download_url"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            name.to_string(),
        );
        if (name.contains("arm64") || name.contains("aarch64")) == want_arm {
            return Some(found);
        }
        fallback.get_or_insert(found);
    }

    fallback
}

pub(crate) fn version_is_newer(latest: &str, current: &str) -> bool {
    parse_semver(latest) > parse_semver(current)
}

pub(crate) fn parse_semver(value: &str) -> (u32, u32, u32) {
    let mut parts = value
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|part| part.trim().parse::<u32>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}
