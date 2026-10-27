use super::*;

impl SessionManager {
    pub fn sftp_download_mode(&mut self, name: &str, mode: TransferMode) {
        if let Some(browser) = &self.sftp {
            let remote = join_remote(&browser.cwd, name);
            let local = browser.local_cwd.join(name);
            self.sftp_download_to(&remote, local, mode);
        }
    }

    pub fn sftp_download_to(&mut self, remote: &str, local: PathBuf, mode: TransferMode) {
        if let Some(browser) = &mut self.sftp {
            let id = browser.next_transfer_id;
            browser.next_transfer_id += 1;
            let name = remote.rsplit('/').next().unwrap_or(remote).to_string();
            browser.transfers.push(TransferItem::new(
                id,
                TransferDirection::Download,
                name.clone(),
                remote.to_string(),
                local.display().to_string(),
            ));
            browser.busy = true;
            browser.status = format!("downloading {name}…");
            if let Err(error) = browser.handle.send(SftpCommand::DownloadWith {
                id,
                remote: remote.into(),
                local,
                mode,
            }) {
                if let Some(item) = browser.transfers.last_mut() {
                    item.status = TransferStatus::Failed;
                    item.error = Some(error.to_string());
                }
            }
        }
    }

    pub fn sftp_upload_to(
        &mut self,
        local: &Path,
        remote: &str,
        mode: TransferMode,
    ) -> Result<(), SessionError> {
        let name = local
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| SessionError::Sftp("invalid local file path".into()))?
            .to_string();
        let browser = self.sftp.as_mut().ok_or(SessionError::NoActiveSshSession)?;
        let id = browser.next_transfer_id;
        browser.next_transfer_id += 1;
        browser.transfers.push(TransferItem::new(
            id,
            TransferDirection::Upload,
            name.clone(),
            local.display().to_string(),
            remote.to_string(),
        ));
        browser.busy = true;
        browser.status = format!("uploading {name}…");
        if let Err(error) = browser.handle.send(SftpCommand::UploadWith {
            id,
            local: local.to_path_buf(),
            remote: remote.into(),
            mode,
        }) {
            if let Some(item) = browser.transfers.last_mut() {
                item.status = TransferStatus::Failed;
                item.error = Some(error.to_string());
            }
            return Err(error.into());
        }
        Ok(())
    }
}
