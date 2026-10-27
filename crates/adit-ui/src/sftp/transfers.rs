use super::*;
use adit_session::TransferMode;
use std::path::PathBuf;

/// Freeze source/destination and connection before a dialog opens: navigation
/// during confirmation must not redirect an approved overwrite to another file.
#[derive(Debug, Clone)]
pub struct PreparedTransfer {
    profile: ProfileId,
    endpoint: String,
    remote: String,
    local: PathBuf,
    direction: TransferDirection,
    exists: bool,
}

fn prepare(app: &AditApp, transfer: PendingTransfer) -> Option<PreparedTransfer> {
    if let PendingTransfer::Prepared(prepared) = transfer {
        return Some(prepared);
    }
    let browser = app.manager.sftp_browser()?;
    let (local, name, direction) = match transfer {
        PendingTransfer::Download(name) => (
            browser.local_cwd.join(&name),
            name,
            TransferDirection::Download,
        ),
        PendingTransfer::UploadLocal(name) => (
            browser.local_cwd.join(&name),
            name,
            TransferDirection::Upload,
        ),
        PendingTransfer::UploadPath(path) => {
            let name = path.file_name()?.to_str()?.to_string();
            (path, name, TransferDirection::Upload)
        }
        PendingTransfer::Prepared(_) => unreachable!(),
    };
    // Names from readdir are data, not paths chosen by the user. Do not let a
    // hostile entry escape the pane directory or name an NTFS alternate stream.
    if name.is_empty()
        || matches!(name.as_str(), "." | "..")
        || name.contains(['/', '\\', '\0', ':'])
        || name.ends_with(['.', ' '])
    {
        return None;
    }
    let exists = match direction {
        TransferDirection::Download => app.manager.sftp_local_file_exists(&name),
        TransferDirection::Upload => app.manager.sftp_remote_file_exists(&name),
    };
    Some(PreparedTransfer {
        profile: browser.profile_id,
        endpoint: browser.endpoint.clone(),
        remote: format!("{}/{}", browser.cwd.trim_end_matches('/'), name),
        local,
        direction,
        exists,
    })
}

pub(crate) fn start_transfers(app: &mut AditApp, transfers: Vec<PendingTransfer>) {
    let prepared: Vec<_> = transfers
        .into_iter()
        .filter_map(|transfer| prepare(app, transfer))
        .collect();
    let clashing = prepared
        .iter()
        .filter(|p| p.exists)
        .map(|p| match p.direction {
            TransferDirection::Download => p.local.display().to_string(),
            TransferDirection::Upload => p.remote.clone(),
        })
        .collect::<Vec<_>>();
    let transfers = prepared
        .into_iter()
        .map(PendingTransfer::Prepared)
        .collect::<Vec<_>>();
    if clashing.is_empty() {
        run_transfers(app, &transfers);
    } else {
        app.sftp_overwrite = Some((transfers, clashing));
    }
}

pub(crate) fn run_transfers(app: &mut AditApp, transfers: &[PendingTransfer]) {
    for transfer in transfers {
        let PendingTransfer::Prepared(plan) = transfer else {
            continue;
        };
        // Permission to replace applies only to paths that actually appeared in
        // the prompt. A new file appearing at any other target must fail safely.
        let mode = if plan.exists {
            TransferMode::Replace
        } else {
            TransferMode::CreateNew
        };
        queue(app, plan, mode);
    }
}

fn queue(app: &mut AditApp, plan: &PreparedTransfer, mode: TransferMode) {
    if !app.manager.sftp_browser().is_some_and(|browser| {
        browser.profile_id == plan.profile && browser.endpoint == plan.endpoint
    }) {
        app.last_error = Some(t("SFTP 连接已改变，请重新选择传输目标").into());
        return;
    }
    match plan.direction {
        TransferDirection::Download => {
            app.manager
                .sftp_download_to(&plan.remote, plan.local.clone(), mode)
        }
        TransferDirection::Upload => {
            match app.manager.sftp_upload_to(&plan.local, &plan.remote, mode) {
                Ok(()) => app.sftp_upload_path.clear(),
                Err(error) => app.last_error = Some(runtime(&error)),
            }
        }
    }
}

pub(crate) fn resume_transfer(app: &mut AditApp, pane: SftpPane, name: String) {
    let request = match pane {
        SftpPane::Remote => PendingTransfer::Download(name),
        SftpPane::Local => PendingTransfer::UploadLocal(name),
    };
    if let Some(plan) = prepare(app, request) {
        queue(app, &plan, TransferMode::Resume);
    }
}

pub(crate) fn begin_save_as(app: &AditApp, name: String) -> Task<Message> {
    let Some(mut plan) = prepare(app, PendingTransfer::Download(name.clone())) else {
        return Task::none();
    };
    let dialog = rfd::AsyncFileDialog::new()
        .set_title(t("下载另存为"))
        .set_file_name(name)
        .set_directory(plan.local.parent().unwrap_or(std::path::Path::new(".")));
    Task::perform(
        async move {
            let Some(file) = dialog.save_file().await else {
                return Ok(None);
            };
            plan.local = file.path().to_path_buf();
            let path = plan.local.clone();
            plan.exists = tokio::task::spawn_blocking(move || path.try_exists())
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            Ok(Some(plan))
        },
        Message::SftpPrepared,
    )
}
